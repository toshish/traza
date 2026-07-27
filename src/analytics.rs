//! Session grouping and LLM cost/token aggregation.
//!
//! Sessions and aggregates are DERIVED views over ordinary spans — no new
//! record type, no format change. What a span contributes (whether it is an
//! LLM call, its model, provider, session, token counts, and cost) is decided
//! by [`crate::semconv`], which recognizes both the OpenLLMetry / OTel GenAI
//! conventions (`gen_ai.*`, `llm.usage.*`, `traceloop.*`) and Traza's native
//! `llm.*` / `session.id` shorthand (docs/llm-semantics.md).
//!
//! Cost model: segments are immutable, so a per-segment rollup is computed at
//! most once per process and cached by path (superseded segments simply fall
//! out of the cache). A query window that only partially overlaps a segment
//! falls back to an exact decode of that one segment — results are always
//! exact, never bucket-approximated. The write buffer is scanned directly on
//! every call (it is at most `flush_spans` entries).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::semconv::{self, LlmFacts};
use crate::{Result, Span, Store};

/// The native attribute carrying a session identifier. It heads the recognized
/// session-key precedence (see [`crate::semconv`]); other keys such as
/// `gen_ai.conversation.id` also group spans into a session.
pub const SESSION_ATTRIBUTE: &str = "session.id";

/// One session's aggregate view.
#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    /// The session identifier shared by the session's spans.
    pub session_id: String,
    /// The attribute key that grouped this session (`session.id`,
    /// `gen_ai.conversation.id`, a `traceloop.association.properties.*` key,
    /// …). Drill-downs filter spans on this key.
    pub session_attribute: String,
    /// Earliest span start in the session.
    pub first_start_ns: u64,
    /// Latest span end in the session.
    pub last_end_ns: u64,
    /// Number of distinct traces containing session spans.
    pub trace_count: usize,
    /// Total spans carrying this session id.
    pub span_count: usize,
    /// Spans recognized as LLM calls (any `llm.*` usage attribute present).
    pub llm_calls: usize,
    /// Summed prompt tokens.
    pub prompt_tokens: u64,
    /// Summed completion tokens.
    pub completion_tokens: u64,
    /// Summed total tokens (explicit `llm.total_tokens`, else prompt+completion).
    pub total_tokens: u64,
    /// Summed cost in USD.
    pub cost_usd: f64,
    /// Spans with status `error`.
    pub error_count: usize,
}

/// One trace inside a session detail view.
#[derive(Clone, Debug, Serialize)]
pub struct SessionTrace {
    /// Trace identifier.
    pub trace_id: String,
    /// Name of the trace's earliest span.
    pub root_name: String,
    /// Earliest span start in the trace (session spans only).
    pub first_start_ns: u64,
    /// Latest span end in the trace (session spans only).
    pub last_end_ns: u64,
    /// Session spans in this trace.
    pub span_count: usize,
    /// Summed total tokens.
    pub total_tokens: u64,
    /// Summed cost in USD.
    pub cost_usd: f64,
    /// Spans with status `error`.
    pub error_count: usize,
}

/// A session plus its per-trace breakdown.
#[derive(Clone, Debug, Serialize)]
pub struct SessionDetail {
    /// The aggregate view.
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// Traces ordered by first activity.
    pub traces: Vec<SessionTrace>,
}

/// How [`Store::sessions`] ranks the population before truncating it.
///
/// This is an engine concern rather than a presentation one. Ranking after
/// truncation answers a different question from the one asked — "the costliest
/// of the hundred most recent" is not "the costliest" — and the difference is
/// invisible in the result, which is what makes it dangerous.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionOrder {
    /// Most recent activity first. The default, and what a listing wants.
    #[default]
    Recent,
    /// Highest summed cost first.
    Cost,
    /// Most error spans first.
    Errors,
    /// Most tokens first.
    Tokens,
}

impl SessionOrder {
    /// Parses the wire name; `None` for an unrecognized one.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "recent" => Some(Self::Recent),
            "cost" => Some(Self::Cost),
            "errors" => Some(Self::Errors),
            "tokens" => Some(Self::Tokens),
            _ => None,
        }
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Cost => "cost",
            Self::Errors => "errors",
            Self::Tokens => "tokens",
        }
    }
}

/// Aggregation dimension for [`Store::llm_aggregate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmGroupBy {
    /// Group by the recognized model (`gen_ai.response.model` →
    /// `gen_ai.request.model` → native `llm.model`).
    Model,
    /// Group by the provider/system (`gen_ai.system`).
    Provider,
    /// Group by the emitting service.
    Service,
    /// Group by the recognized session identifier.
    Session,
    /// Group by UTC calendar day of the span start.
    Day,
}

impl LlmGroupBy {
    /// Parses the wire name used by `GET /v1/stats/llm?group_by=`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            "service" => Some(Self::Service),
            "session" => Some(Self::Session),
            "day" => Some(Self::Day),
            _ => None,
        }
    }
}

/// One row of an LLM aggregation.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LlmAggregateRow {
    /// The group key (model name, service, session id, or YYYY-MM-DD day).
    pub key: String,
    /// All spans in the group.
    pub spans: usize,
    /// Spans recognized as LLM calls.
    pub llm_calls: usize,
    /// Summed prompt tokens.
    pub prompt_tokens: u64,
    /// Summed completion tokens.
    pub completion_tokens: u64,
    /// Summed total tokens.
    pub total_tokens: u64,
    /// Summed cost in USD.
    pub cost_usd: f64,
    /// Spans with status `error`.
    pub error_count: usize,
    /// Summed duration of LLM calls, for average latency (`/ llm_calls`).
    pub llm_duration_ns: u64,
}

// ------------------------------------------------------------------ counters

#[derive(Clone, Debug, Default)]
struct Counters {
    spans: usize,
    llm_calls: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cost_usd: f64,
    errors: usize,
    llm_duration_ns: u64,
}

impl Counters {
    /// Folds one span's normalized [`LlmFacts`] in. Facts are computed once
    /// per span in [`SegmentRollup::absorb`] and shared across every group the
    /// span joins, so the semconv scan runs once, not once per group.
    fn absorb(&mut self, span: &Span, facts: &LlmFacts) {
        self.spans = self.spans.saturating_add(1);
        if span.status == "error" {
            self.errors = self.errors.saturating_add(1);
        }
        if facts.is_llm {
            self.llm_calls = self.llm_calls.saturating_add(1);
            self.llm_duration_ns = self
                .llm_duration_ns
                .saturating_add(span.end_time_ns.saturating_sub(span.start_time_ns));
        }
        self.prompt_tokens = self
            .prompt_tokens
            .saturating_add(facts.prompt_tokens.unwrap_or(0));
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(facts.completion_tokens.unwrap_or(0));
        self.total_tokens = self.total_tokens.saturating_add(facts.total());
        self.cost_usd = finite_saturating_add(self.cost_usd, facts.cost_usd.unwrap_or(0.0));
    }

    fn merge(&mut self, other: &Counters) {
        self.spans = self.spans.saturating_add(other.spans);
        self.llm_calls = self.llm_calls.saturating_add(other.llm_calls);
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cost_usd = finite_saturating_add(self.cost_usd, other.cost_usd);
        self.errors = self.errors.saturating_add(other.errors);
        self.llm_duration_ns = self.llm_duration_ns.saturating_add(other.llm_duration_ns);
    }
}

fn finite_saturating_add(left: f64, right: f64) -> f64 {
    let total = left + right;
    if total.is_finite() {
        total
    } else if total.is_sign_negative() {
        f64::MIN
    } else {
        f64::MAX
    }
}

#[derive(Clone, Debug, Default)]
struct SessionCounters {
    counters: Counters,
    first_start_ns: u64,
    last_end_ns: u64,
    traces: HashSet<String>,
    /// The recognized session key that grouped this session; the
    /// highest-precedence key seen wins when spans differ (see [`prefer_key`]).
    session_key: Option<&'static str>,
}

impl SessionCounters {
    fn absorb(&mut self, span: &Span, facts: &LlmFacts) {
        if self.counters.spans == 0 || span.start_time_ns < self.first_start_ns {
            self.first_start_ns = span.start_time_ns;
        }
        if span.end_time_ns > self.last_end_ns {
            self.last_end_ns = span.end_time_ns;
        }
        self.traces.insert(span.trace_id.clone());
        self.session_key = prefer_key(self.session_key, facts.session_key);
        self.counters.absorb(span, facts);
    }

    fn merge(&mut self, other: &SessionCounters) {
        if self.counters.spans == 0 || other.first_start_ns < self.first_start_ns {
            self.first_start_ns = other.first_start_ns;
        }
        if other.last_end_ns > self.last_end_ns {
            self.last_end_ns = other.last_end_ns;
        }
        self.traces.extend(other.traces.iter().cloned());
        self.session_key = prefer_key(self.session_key, other.session_key);
        self.counters.merge(&other.counters);
    }

    /// The session key to report, defaulting to the native `session.id`.
    fn attribute(&self) -> String {
        self.session_key.unwrap_or(SESSION_ATTRIBUTE).to_owned()
    }
}

/// Keeps the higher-precedence session key (earlier in the recognized order).
fn prefer_key(
    current: Option<&'static str>,
    candidate: Option<&'static str>,
) -> Option<&'static str> {
    let rank = |key: Option<&str>| {
        key.and_then(|k| semconv::SESSION_KEYS.iter().position(|c| *c == k))
            .unwrap_or(usize::MAX)
    };
    match (current, candidate) {
        (None, other) | (other, None) => other,
        (Some(_), Some(_)) if rank(candidate) < rank(current) => candidate,
        (current, _) => current,
    }
}

/// The cached per-segment rollup (segments are immutable, so this is valid
/// for the segment's whole lifetime).
#[derive(Debug, Default)]
pub(crate) struct SegmentRollup {
    min_start_ns: u64,
    max_start_ns: u64,
    by_model: HashMap<String, Counters>,
    by_provider: HashMap<String, Counters>,
    by_service: HashMap<String, Counters>,
    by_day: BTreeMap<String, Counters>,
    by_session_key: HashMap<String, Counters>,
    sessions: HashMap<String, SessionCounters>,
    /// FNV-1a hashes of every (trace_id, span_id) in the rollup: the
    /// supersede prefilter. A key replaced in a NEWER source makes this
    /// rollup unusable as-is (its counters include the stale version).
    key_hashes: HashSet<u64>,
    /// `$payload` references held by any span in the rollup — the live set
    /// that protects payload files from the TTL sweep.
    pub(crate) payload_refs: HashSet<String>,
}

impl SegmentRollup {
    fn build(spans: &[Span]) -> Self {
        let mut rollup = Self {
            min_start_ns: u64::MAX,
            ..Self::default()
        };
        for span in spans {
            rollup.absorb(span);
        }
        if rollup.min_start_ns == u64::MAX {
            rollup.min_start_ns = 0;
        }
        rollup
    }

    fn absorb(&mut self, span: &Span) {
        self.min_start_ns = self.min_start_ns.min(span.start_time_ns);
        self.max_start_ns = self.max_start_ns.max(span.start_time_ns);
        self.key_hashes
            .insert(key_hash(&span.trace_id, &span.span_id));
        collect_payload_refs(span, &mut self.payload_refs);
        // One semconv scan, shared across every group this span joins.
        let facts = semconv::facts(&span.attributes);
        if let Some(model) = &facts.model {
            self.by_model
                .entry(model.clone())
                .or_default()
                .absorb(span, &facts);
        }
        if let Some(provider) = &facts.provider {
            self.by_provider
                .entry(provider.clone())
                .or_default()
                .absorb(span, &facts);
        }
        self.by_service
            .entry(span.service.clone())
            .or_default()
            .absorb(span, &facts);
        self.by_day
            .entry(day_bucket(span.start_time_ns))
            .or_default()
            .absorb(span, &facts);
        if let Some(session) = &facts.session {
            self.by_session_key
                .entry(session.clone())
                .or_default()
                .absorb(span, &facts);
            self.sessions
                .entry(session.clone())
                .or_default()
                .absorb(span, &facts);
        }
    }
}

// ------------------------------------------------------------ span helpers

/// FNV-1a over the primary key. Used only as a PREFILTER: a hash collision
/// can force an unnecessary exact re-scan, never a wrong count.
fn key_hash(trace_id: &str, span_id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in trace_id
        .as_bytes()
        .iter()
        .chain([0_u8].iter())
        .chain(span_id.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Collects `$payload` references from span and event attributes.
fn collect_payload_refs(span: &Span, refs: &mut HashSet<String>) {
    let mut scan = |attributes: &Map<String, Value>| {
        for value in attributes.values() {
            if let Some(reference) = value
                .get(crate::payload::PAYLOAD_KEY)
                .and_then(Value::as_str)
            {
                refs.insert(reference.to_owned());
            }
        }
    };
    scan(&span.attributes);
    for event in &span.events {
        scan(&event.attributes);
    }
}

/// UTC calendar day (YYYY-MM-DD) of a nanosecond timestamp, dependency-free
/// (civil-from-days, H. Hinnant's algorithm).
fn day_bucket(start_ns: u64) -> String {
    let days = (start_ns / 1_000_000_000 / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

fn in_window(start_ns: u64, since: Option<u64>, until: Option<u64>) -> bool {
    since.map_or(true, |bound| start_ns >= bound) && until.map_or(true, |bound| start_ns <= bound)
}

// -------------------------------------------------------------- store API

impl Store {
    /// Lists sessions active in the window, ranked by `order`, truncated to
    /// `limit`.
    ///
    /// **The ranking happens here, over the complete population, and not in
    /// the caller.** A caller that asks for the costliest session cannot get a
    /// correct answer by re-sorting a page this returned: the page was chosen
    /// by whatever `order` produced it, so an expensive session outside it is
    /// invisible rather than merely lower down. The fold below already
    /// materializes every session in the window — ordering by any of these
    /// keys is a comparator change, not extra work.
    pub fn sessions(
        &self,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
        limit: usize,
        order: SessionOrder,
    ) -> Result<Vec<SessionSummary>> {
        let mut merged: HashMap<String, SessionCounters> = HashMap::new();
        self.fold_analytics(since_ns, until_ns, |rollup| {
            for (id, counters) in &rollup.sessions {
                merged.entry(id.clone()).or_default().merge(counters);
            }
        })?;
        let mut sessions: Vec<SessionSummary> = merged
            .into_iter()
            .map(|(session_id, entry)| SessionSummary {
                session_id,
                session_attribute: entry.attribute(),
                first_start_ns: entry.first_start_ns,
                last_end_ns: entry.last_end_ns,
                trace_count: entry.traces.len(),
                span_count: entry.counters.spans,
                llm_calls: entry.counters.llm_calls,
                prompt_tokens: entry.counters.prompt_tokens,
                completion_tokens: entry.counters.completion_tokens,
                total_tokens: entry.counters.total_tokens,
                cost_usd: entry.counters.cost_usd,
                error_count: entry.counters.errors,
            })
            .collect();
        // The session id breaks every tie, so a page is deterministic even
        // when a hundred sessions report the same cost.
        sessions.sort_by(|a, b| {
            let ranked = match order {
                SessionOrder::Recent => b.last_end_ns.cmp(&a.last_end_ns),
                SessionOrder::Cost => b
                    .cost_usd
                    .partial_cmp(&a.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal),
                SessionOrder::Errors => b.error_count.cmp(&a.error_count),
                SessionOrder::Tokens => b.total_tokens.cmp(&a.total_tokens),
            };
            ranked.then_with(|| a.session_id.cmp(&b.session_id))
        });
        sessions.truncate(limit);
        Ok(sessions)
    }

    /// Every span belonging to `session_id`, resolved across ALL recognized
    /// session keys (`session.id`, `gen_ai.conversation.id`, a
    /// `traceloop.association.properties.*` key). Each candidate key is an
    /// index-served point query; the union is re-checked against the semconv
    /// precedence so a span whose RESOLVED session differs (for example it
    /// carries both a native `session.id` and a matching
    /// `gen_ai.conversation.id`) lands in exactly one session. This is what
    /// makes a mixed-convention session queryable as a whole.
    pub(crate) fn resolve_session_spans(&self, session_id: &str) -> Result<Vec<Span>> {
        let candidates =
            self.query_attribute_union(&semconv::SESSION_KEYS, &session_values(session_id))?;
        Ok(narrow_to_session(candidates, session_id))
    }

    /// One session with its per-trace breakdown, or `None` when no span
    /// carries the id.
    pub fn session(&self, session_id: &str) -> Result<Option<SessionDetail>> {
        let spans = self.resolve_session_spans(session_id)?;
        if spans.is_empty() {
            return Ok(None);
        }
        let mut session = SessionCounters::default();
        let mut traces: BTreeMap<String, (Vec<&Span>, Counters)> = BTreeMap::new();
        for span in &spans {
            let facts = semconv::facts(&span.attributes);
            session.absorb(span, &facts);
            let entry = traces.entry(span.trace_id.clone()).or_default();
            entry.0.push(span);
            entry.1.absorb(span, &facts);
        }
        let mut trace_rows: Vec<SessionTrace> = traces
            .into_iter()
            .map(|(trace_id, (members, counters))| {
                let root = members
                    .iter()
                    .min_by_key(|span| span.start_time_ns)
                    .expect("non-empty trace group");
                SessionTrace {
                    trace_id,
                    root_name: root.name.clone(),
                    first_start_ns: root.start_time_ns,
                    last_end_ns: members
                        .iter()
                        .map(|span| span.end_time_ns)
                        .max()
                        .unwrap_or(0),
                    span_count: counters.spans,
                    total_tokens: counters.total_tokens,
                    cost_usd: counters.cost_usd,
                    error_count: counters.errors,
                }
            })
            .collect();
        trace_rows.sort_by_key(|row| row.first_start_ns);
        Ok(Some(SessionDetail {
            summary: SessionSummary {
                session_id: session_id.to_owned(),
                session_attribute: session.attribute(),
                first_start_ns: session.first_start_ns,
                last_end_ns: session.last_end_ns,
                trace_count: session.traces.len(),
                span_count: session.counters.spans,
                llm_calls: session.counters.llm_calls,
                prompt_tokens: session.counters.prompt_tokens,
                completion_tokens: session.counters.completion_tokens,
                total_tokens: session.counters.total_tokens,
                cost_usd: session.counters.cost_usd,
                error_count: session.counters.errors,
            },
            traces: trace_rows,
        }))
    }

    /// Aggregates LLM usage over the window, grouped by `group_by`, sorted by
    /// cost (then tokens, then key).
    pub fn llm_aggregate(
        &self,
        group_by: LlmGroupBy,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
    ) -> Result<Vec<LlmAggregateRow>> {
        let mut merged: HashMap<String, Counters> = HashMap::new();
        self.fold_analytics(since_ns, until_ns, |rollup| {
            let groups: Box<dyn Iterator<Item = (&String, &Counters)>> = match group_by {
                LlmGroupBy::Model => Box::new(rollup.by_model.iter()),
                LlmGroupBy::Provider => Box::new(rollup.by_provider.iter()),
                LlmGroupBy::Service => Box::new(rollup.by_service.iter()),
                LlmGroupBy::Session => Box::new(rollup.by_session_key.iter()),
                LlmGroupBy::Day => Box::new(rollup.by_day.iter()),
            };
            for (key, counters) in groups {
                merged.entry(key.clone()).or_default().merge(counters);
            }
        })?;
        let mut rows: Vec<LlmAggregateRow> = merged
            .into_iter()
            .map(|(key, counters)| LlmAggregateRow {
                key,
                spans: counters.spans,
                llm_calls: counters.llm_calls,
                prompt_tokens: counters.prompt_tokens,
                completion_tokens: counters.completion_tokens,
                total_tokens: counters.total_tokens,
                cost_usd: counters.cost_usd,
                error_count: counters.errors,
                llm_duration_ns: counters.llm_duration_ns,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then_with(|| b.total_tokens.cmp(&a.total_tokens))
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(rows)
    }

    /// Folds every in-window span group into `visit`, honoring the
    /// (trace_id, span_id) primary key: a span re-ingested later exists only
    /// in its NEWEST version, so segments are walked newest-first carrying
    /// the set of keys already seen (buffer first — it always wins). A
    /// cached rollup is used verbatim only when the window covers it AND no
    /// key in it was seen in a newer source (FNV prefilter; a collision just
    /// forces an exact re-scan). Found live: a replaced span kept both
    /// versions in the aggregates — 2 calls, 30 tokens, $0.30 where the
    /// truth was 1 call, 20 tokens, $0.20.
    fn fold_analytics(
        &self,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
        mut visit: impl FnMut(&SegmentRollup),
    ) -> Result<()> {
        // Lock order: writer before segments (see Store field docs).
        let writer = self.lock_writer()?;
        let buffered: Vec<Span> = writer
            .spans
            .iter()
            .filter(|span| in_window(span.start_time_ns, since_ns, until_ns))
            .map(|span| Span::clone(span))
            .collect();
        // ALL buffer keys supersede segment copies, in-window or not.
        let buffer_keys: HashSet<(String, String)> = writer.index.keys().cloned().collect();
        drop(writer);
        if !buffered.is_empty() {
            visit(&SegmentRollup::build(&buffered));
        }
        let mut seen_hashes: HashSet<u64> = buffer_keys
            .iter()
            .map(|(trace_id, span_id)| key_hash(trace_id, span_id))
            .collect();

        let segments = self.lock_segments()?;
        let mut live_paths: HashSet<PathBuf> = HashSet::new();
        // Newest first: paths are zero-padded, so path order is flush order.
        for (position, segment) in segments.iter().enumerate().rev() {
            live_paths.insert(segment.path.clone());
            let rollup = self.segment_rollup(segment)?;
            let overlaps = since_ns.map_or(true, |bound| rollup.max_start_ns >= bound)
                && until_ns.map_or(true, |bound| rollup.min_start_ns <= bound);
            if !overlaps {
                seen_hashes.extend(rollup.key_hashes.iter().copied());
                continue;
            }
            let fully_inside = in_window(rollup.min_start_ns, since_ns, until_ns)
                && in_window(rollup.max_start_ns, since_ns, until_ns);
            let possibly_superseded = rollup
                .key_hashes
                .iter()
                .any(|hash| seen_hashes.contains(hash));
            if fully_inside && !possibly_superseded {
                visit(&rollup);
                seen_hashes.extend(rollup.key_hashes.iter().copied());
                continue;
            }
            // Exact path: decode this segment, dropping out-of-window spans
            // and spans whose key exists in the buffer or a newer segment.
            let mut survivors: Vec<Span> = Vec::new();
            for span in segment.spans_parsed()? {
                if !in_window(span.start_time_ns, since_ns, until_ns) {
                    continue;
                }
                let key = (span.trace_id.clone(), span.span_id.clone());
                if buffer_keys.contains(&key) {
                    continue;
                }
                let mut superseded = false;
                for newer in segments.iter().skip(position + 1) {
                    if newer.contains_key(&span.trace_id, &span.span_id)? {
                        superseded = true;
                        break;
                    }
                }
                if !superseded {
                    survivors.push(span);
                }
            }
            if !survivors.is_empty() {
                visit(&SegmentRollup::build(&survivors));
            }
            seen_hashes.extend(rollup.key_hashes.iter().copied());
        }
        // Superseded segments drop out of the cache with their paths.
        self.rollups
            .lock()
            .map_err(|_| crate::Error::LockPoisoned("rollups"))?
            .retain(|path, _| live_paths.contains(path));
        Ok(())
    }

    /// Every `$payload` reference held by a live span (buffer + all
    /// segments): the protection set for the TTL payload sweep. Superseded
    /// span versions may contribute references; that over-protection only
    /// delays deletion, never loses data.
    pub(crate) fn live_payload_refs(&self) -> Result<HashSet<String>> {
        let mut refs = HashSet::new();
        let writer = self.lock_writer()?;
        for span in &writer.spans {
            collect_payload_refs(span, &mut refs);
        }
        drop(writer);
        let segments = self.lock_segments()?;
        for segment in segments.iter() {
            refs.extend(self.segment_rollup(segment)?.payload_refs.iter().cloned());
        }
        Ok(refs)
    }

    /// The segment's cached rollup, building it on first use.
    fn segment_rollup(&self, segment: &crate::Segment) -> Result<Arc<SegmentRollup>> {
        {
            let cache = self
                .rollups
                .lock()
                .map_err(|_| crate::Error::LockPoisoned("rollups"))?;
            if let Some(rollup) = cache.get(&segment.path) {
                return Ok(Arc::clone(rollup));
            }
        }
        let rollup = Arc::new(SegmentRollup::build(&segment.spans_parsed()?));
        self.rollups
            .lock()
            .map_err(|_| crate::Error::LockPoisoned("rollups"))?
            .insert(segment.path.clone(), Arc::clone(&rollup));
        Ok(rollup)
    }
}

/// [`Store::resolve_session_spans`] against a buffer and segment set the
/// caller is holding still — the pinned-view half of the same resolution, used
/// by [`crate::SnapshotView`] so a paged export can filter by session.
pub(crate) fn resolve_session_spans_in(
    buffer: &crate::WriteBuffer,
    segments: &[Arc<crate::Segment>],
    session_id: &str,
) -> Result<Vec<Span>> {
    let candidates = crate::attribute_union_view(
        buffer,
        segments,
        &semconv::SESSION_KEYS,
        &session_values(session_id),
    )?;
    Ok(narrow_to_session(candidates, session_id))
}

/// Every JSON encoding that normalizes to the session id `session_id`.
///
/// Session normalization stringifies numeric attributes, so a producer that
/// sent `"gen_ai.conversation.id": 4711` yields the session id "4711".
/// Matching only the JSON string would then list that session and refuse to
/// open it.
fn session_values(session_id: &str) -> Vec<Value> {
    let mut values = vec![Value::String(session_id.to_owned())];
    if let Ok(number) = session_id.parse::<u64>() {
        values.push(Value::from(number));
    } else if let Ok(number) = session_id.parse::<i64>() {
        values.push(Value::from(number));
    } else if let Ok(number) = session_id.parse::<f64>() {
        if number.is_finite() {
            if let Some(value) = serde_json::Number::from_f64(number) {
                values.push(Value::Number(value));
            }
        }
    }
    values
}

/// The attribute union over-selects: a span may carry a matching value under a
/// LOWER-precedence key while its resolved session is something else.
fn narrow_to_session(candidates: Vec<Span>, session_id: &str) -> Vec<Span> {
    candidates
        .into_iter()
        .filter(|span| semconv::facts(&span.attributes).session.as_deref() == Some(session_id))
        .collect()
}
