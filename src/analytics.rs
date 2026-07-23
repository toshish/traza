//! Session grouping and LLM cost/token aggregation.
//!
//! Sessions and aggregates are DERIVED views over ordinary spans — no new
//! record type, no format change. A span joins a session by carrying the
//! `session.id` attribute; token/cost figures come from the documented
//! `llm.*` attributes (docs/llm-semantics.md).
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

use crate::{Result, Span, Store};

/// Attribute carrying the session identifier (see docs/llm-semantics.md).
pub const SESSION_ATTRIBUTE: &str = "session.id";

const ATTR_MODEL: &str = "llm.model";
const ATTR_PROMPT_TOKENS: &str = "llm.prompt_tokens";
const ATTR_COMPLETION_TOKENS: &str = "llm.completion_tokens";
const ATTR_TOTAL_TOKENS: &str = "llm.total_tokens";
const ATTR_COST: &str = "llm.cost_usd";

/// One session's aggregate view.
#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    /// The `session.id` attribute value shared by the session's spans.
    pub session_id: String,
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

/// Aggregation dimension for [`Store::llm_aggregate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmGroupBy {
    /// Group by the `llm.model` attribute.
    Model,
    /// Group by the emitting service.
    Service,
    /// Group by the `session.id` attribute.
    Session,
    /// Group by UTC calendar day of the span start.
    Day,
}

impl LlmGroupBy {
    /// Parses the wire name used by `GET /v1/stats/llm?group_by=`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "model" => Some(Self::Model),
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
    fn absorb(&mut self, span: &Span) {
        self.spans += 1;
        if span.status == "error" {
            self.errors += 1;
        }
        let prompt = attr_u64(&span.attributes, ATTR_PROMPT_TOKENS);
        let completion = attr_u64(&span.attributes, ATTR_COMPLETION_TOKENS);
        let explicit_total = attr_u64(&span.attributes, ATTR_TOTAL_TOKENS);
        let cost = attr_f64(&span.attributes, ATTR_COST);
        let is_llm = span.attributes.contains_key(ATTR_MODEL)
            || prompt.is_some()
            || completion.is_some()
            || explicit_total.is_some();
        if is_llm {
            self.llm_calls += 1;
            self.llm_duration_ns += span.end_time_ns.saturating_sub(span.start_time_ns);
        }
        self.prompt_tokens += prompt.unwrap_or(0);
        self.completion_tokens += completion.unwrap_or(0);
        self.total_tokens +=
            explicit_total.unwrap_or(prompt.unwrap_or(0) + completion.unwrap_or(0));
        self.cost_usd += cost.unwrap_or(0.0);
    }

    fn merge(&mut self, other: &Counters) {
        self.spans += other.spans;
        self.llm_calls += other.llm_calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cost_usd += other.cost_usd;
        self.errors += other.errors;
        self.llm_duration_ns += other.llm_duration_ns;
    }
}

#[derive(Clone, Debug, Default)]
struct SessionCounters {
    counters: Counters,
    first_start_ns: u64,
    last_end_ns: u64,
    traces: HashSet<String>,
}

impl SessionCounters {
    fn absorb(&mut self, span: &Span) {
        if self.counters.spans == 0 || span.start_time_ns < self.first_start_ns {
            self.first_start_ns = span.start_time_ns;
        }
        if span.end_time_ns > self.last_end_ns {
            self.last_end_ns = span.end_time_ns;
        }
        self.traces.insert(span.trace_id.clone());
        self.counters.absorb(span);
    }

    fn merge(&mut self, other: &SessionCounters) {
        if self.counters.spans == 0 || other.first_start_ns < self.first_start_ns {
            self.first_start_ns = other.first_start_ns;
        }
        if other.last_end_ns > self.last_end_ns {
            self.last_end_ns = other.last_end_ns;
        }
        self.traces.extend(other.traces.iter().cloned());
        self.counters.merge(&other.counters);
    }
}

/// The cached per-segment rollup (segments are immutable, so this is valid
/// for the segment's whole lifetime).
#[derive(Debug, Default)]
pub(crate) struct SegmentRollup {
    min_start_ns: u64,
    max_start_ns: u64,
    by_model: HashMap<String, Counters>,
    by_service: HashMap<String, Counters>,
    by_day: BTreeMap<String, Counters>,
    by_session_key: HashMap<String, Counters>,
    sessions: HashMap<String, SessionCounters>,
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
        if let Some(model) = attr_str(&span.attributes, ATTR_MODEL) {
            self.by_model.entry(model).or_default().absorb(span);
        }
        self.by_service
            .entry(span.service.clone())
            .or_default()
            .absorb(span);
        self.by_day
            .entry(day_bucket(span.start_time_ns))
            .or_default()
            .absorb(span);
        if let Some(session) = attr_str(&span.attributes, SESSION_ATTRIBUTE) {
            self.by_session_key
                .entry(session.clone())
                .or_default()
                .absorb(span);
            self.sessions.entry(session).or_default().absorb(span);
        }
    }
}

// ------------------------------------------------------------ span helpers

fn attr_str(attributes: &Map<String, Value>, key: &str) -> Option<String> {
    match attributes.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Numeric attribute, tolerant of numeric strings (native-JSON producers and
/// some OTLP exporters stringify counters).
fn attr_u64(attributes: &Map<String, Value>, key: &str) -> Option<u64> {
    match attributes.get(key)? {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|value| value.max(0.0) as u64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn attr_f64(attributes: &Map<String, Value>, key: &str) -> Option<f64> {
    match attributes.get(key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
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
    /// Lists sessions active in the window, most recent activity first.
    pub fn sessions(
        &self,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
        limit: usize,
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
        sessions.sort_by(|a, b| {
            b.last_end_ns
                .cmp(&a.last_end_ns)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        sessions.truncate(limit);
        Ok(sessions)
    }

    /// One session with its per-trace breakdown, or `None` when no span
    /// carries the id.
    pub fn session(&self, session_id: &str) -> Result<Option<SessionDetail>> {
        let spans = self.query(&crate::SpanFilter {
            attributes: vec![(
                SESSION_ATTRIBUTE.to_owned(),
                Value::String(session_id.into()),
            )],
            limit: None,
            ..crate::SpanFilter::default()
        })?;
        if spans.is_empty() {
            return Ok(None);
        }
        let mut session = SessionCounters::default();
        let mut traces: BTreeMap<String, (Vec<&Span>, Counters)> = BTreeMap::new();
        for span in &spans {
            session.absorb(span);
            let entry = traces.entry(span.trace_id.clone()).or_default();
            entry.0.push(span);
            entry.1.absorb(span);
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

    /// Folds every in-window span group into `visit`: cached rollups for
    /// fully-covered segments, exact single-segment rollups for partially
    /// covered ones, and a live rollup of the write buffer.
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
            .cloned()
            .collect();
        drop(writer);
        if !buffered.is_empty() {
            visit(&SegmentRollup::build(&buffered));
        }

        let segments = self.lock_segments()?;
        let mut live_paths: HashSet<PathBuf> = HashSet::new();
        for segment in segments.iter() {
            live_paths.insert(segment.path.clone());
            let rollup = self.segment_rollup(segment)?;
            let fully_inside = in_window(rollup.min_start_ns, since_ns, until_ns)
                && in_window(rollup.max_start_ns, since_ns, until_ns);
            if fully_inside {
                visit(&rollup);
                continue;
            }
            let overlaps = since_ns.map_or(true, |bound| rollup.max_start_ns >= bound)
                && until_ns.map_or(true, |bound| rollup.min_start_ns <= bound);
            if !overlaps {
                continue;
            }
            // Boundary segment: exact per-query rollup of just the in-window
            // spans (decode cost confined to window edges).
            let spans: Vec<Span> = segment
                .spans_parsed()?
                .into_iter()
                .filter(|span| in_window(span.start_time_ns, since_ns, until_ns))
                .collect();
            if !spans.is_empty() {
                visit(&SegmentRollup::build(&spans));
            }
        }
        // Superseded segments drop out of the cache with their paths.
        self.rollups
            .lock()
            .map_err(|_| crate::Error::LockPoisoned("rollups"))?
            .retain(|path, _| live_paths.contains(path));
        Ok(())
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
