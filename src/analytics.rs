//! Session grouping and LLM cost/token aggregation.
//!
//! Sessions and aggregates are DERIVED views over ordinary spans — no new
//! record type, no format change. What a span contributes (whether it is an
//! LLM call, its model, provider, session, token counts, and cost) is decided
//! by [`crate::semconv`], which recognizes both the OpenLLMetry / OTel GenAI
//! conventions (`gen_ai.*`, `llm.usage.*`, `traceloop.*`) and Traza's native
//! `llm.*` / `session.id` shorthand (docs/llm-semantics.md).
//!
//! Cost model, in the order a query tries things:
//!
//! 1. **The in-memory rollup cache**, keyed by segment path. Segments are
//!    immutable, so a rollup is valid for the segment's whole lifetime;
//!    superseded segments fall out of the cache with their paths.
//! 2. **The on-disk rollup sidecar** (`src/rollup_file.rs`), written beside
//!    the segment at seal time. This is what a RESTART lives on: the
//!    in-memory cache is empty in a fresh process, so without a persisted
//!    rollup the first aggregation after every restart decodes the entire
//!    corpus.
//! 3. **Decoding the segment**, which also writes the sidecar so the next
//!    process does not repeat it.
//!
//! A query window that only partially overlaps a segment cannot use a rollup
//! at all — the counters cover spans outside the window — so it decodes that
//! segment. It decodes only the window's slice: records are stored in
//! ascending start-time order, so the window is a contiguous ordinal range
//! that `Segment::ordinal_range_for_window` finds by binary search.
//!
//! Results are always exact, never bucket-approximated. The write buffer is
//! scanned directly on every call (it is at most `flush_spans` entries) and
//! is the one part with no cache, because it is still changing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

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
    /// The tenant whose spans formed this session; omitted for the default
    /// tenant. Session identity is `(tenant, session_id)` — two tenants
    /// reusing an id are two sessions, never one merged row.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tenant: String,
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
    /// Summed cost in USD, metered and derived together.
    pub cost_usd: f64,
    /// The part of `cost_usd` derived from the configured pricing table
    /// rather than reported by a span.
    pub cost_derived_usd: f64,
    /// LLM calls whose cost the span itself reported.
    pub cost_metered_calls: usize,
    /// LLM calls priced from the configured table.
    pub cost_derived_calls: usize,
    /// LLM calls with no cost from either source — an unpriced model, or a
    /// call that reported only a total token count. **These contribute
    /// nothing to `cost_usd`, so a total with a non-zero count here is an
    /// undercount rather than a complete answer.**
    pub cost_unpriced_calls: usize,
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
    /// Summed cost in USD, metered and derived together.
    pub cost_usd: f64,
    /// The part of `cost_usd` derived from the configured pricing table
    /// rather than reported by a span.
    pub cost_derived_usd: f64,
    /// LLM calls whose cost the span itself reported.
    pub cost_metered_calls: usize,
    /// LLM calls priced from the configured table.
    pub cost_derived_calls: usize,
    /// LLM calls with no cost from either source — an unpriced model, or a
    /// call that reported only a total token count. **These contribute
    /// nothing to `cost_usd`, so a total with a non-zero count here is an
    /// undercount rather than a complete answer.**
    pub cost_unpriced_calls: usize,
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
    /// Summed cost in USD, metered and derived together.
    pub cost_usd: f64,
    /// The part of `cost_usd` derived from the configured pricing table
    /// rather than reported by a span.
    pub cost_derived_usd: f64,
    /// LLM calls whose cost the span itself reported.
    pub cost_metered_calls: usize,
    /// LLM calls priced from the configured table.
    pub cost_derived_calls: usize,
    /// LLM calls with no cost from either source — an unpriced model, or a
    /// call that reported only a total token count. **These contribute
    /// nothing to `cost_usd`, so a total with a non-zero count here is an
    /// undercount rather than a complete answer.**
    pub cost_unpriced_calls: usize,
    /// Spans with status `error`.
    pub error_count: usize,
    /// Summed duration of LLM calls, for average latency (`/ llm_calls`).
    pub llm_duration_ns: u64,
}

// ------------------------------------------------------------------ counters

#[derive(Clone, Debug, Default)]
pub(crate) struct Counters {
    pub(crate) spans: usize,
    pub(crate) llm_calls: usize,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_usd: f64,
    /// The part of `cost_usd` that came from a pricing table rather than from
    /// a span. Summed separately because a total that mixes measurement with
    /// estimate is not interpretable without knowing the mix.
    pub(crate) cost_derived_usd: f64,
    /// LLM calls whose cost the span itself reported.
    pub(crate) cost_metered_calls: usize,
    /// LLM calls whose cost came from the pricing table.
    pub(crate) cost_derived_calls: usize,
    /// LLM calls with no cost from either source — an unpriced model, or a
    /// call that reported only a total token count.
    pub(crate) cost_unpriced_calls: usize,
    pub(crate) errors: usize,
    pub(crate) llm_duration_ns: u64,
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
        // Provenance is counted, never inferred from the dollars. A zero-rate
        // model is priced and contributes nothing; an unpriced call also
        // contributes nothing — and reading `cost_derived_usd == 0` as
        // "measured" would call both of them metered, which is false twice.
        match (facts.cost_usd, facts.cost_derived) {
            (Some(cost), true) => {
                self.cost_derived_usd = finite_saturating_add(self.cost_derived_usd, cost);
                self.cost_derived_calls = self.cost_derived_calls.saturating_add(1);
            }
            (Some(_), false) => self.cost_metered_calls = self.cost_metered_calls.saturating_add(1),
            // Only LLM calls can be *un*priced. A span carrying no model, no
            // tokens and no cost is not a call that failed to get a price.
            (None, _) if facts.is_llm => {
                self.cost_unpriced_calls = self.cost_unpriced_calls.saturating_add(1);
            }
            (None, _) => {}
        }
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
        self.cost_derived_usd =
            finite_saturating_add(self.cost_derived_usd, other.cost_derived_usd);
        self.cost_metered_calls = self
            .cost_metered_calls
            .saturating_add(other.cost_metered_calls);
        self.cost_derived_calls = self
            .cost_derived_calls
            .saturating_add(other.cost_derived_calls);
        self.cost_unpriced_calls = self
            .cost_unpriced_calls
            .saturating_add(other.cost_unpriced_calls);
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
pub(crate) struct SessionCounters {
    pub(crate) counters: Counters,
    pub(crate) first_start_ns: u64,
    pub(crate) last_end_ns: u64,
    pub(crate) traces: HashSet<String>,
    /// The recognized session key that grouped this session; the
    /// highest-precedence key seen wins when spans differ (see [`prefer_key`]).
    pub(crate) session_key: Option<&'static str>,
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
    pub(crate) min_start_ns: u64,
    pub(crate) max_start_ns: u64,
    /// Inclusive range of span END times.
    ///
    /// Separate from the start range because TTL expires on `end_time_ns`,
    /// and nothing constrains a span's end to fall inside the segment's start
    /// range — a long span can end well after the last span in its segment
    /// started. Carrying it lets expiry decide whether a segment holds
    /// anything expirable without decoding it.
    pub(crate) min_end_ns: u64,
    pub(crate) max_end_ns: u64,
    pub(crate) by_model: HashMap<String, Counters>,
    pub(crate) by_provider: HashMap<String, Counters>,
    pub(crate) by_service: HashMap<String, Counters>,
    pub(crate) by_day: BTreeMap<String, Counters>,
    /// Keyed `(tenant, session_id)` — STRUCTURALLY, not as a joined display
    /// string. A joined key would collide: a default-tenant session literally
    /// named `acme/checkout` is not tenant `acme`'s session `checkout`, and a
    /// map that cannot tell them apart silently merges two tenants' counters
    /// in a persisted sidecar.
    pub(crate) by_session_key: HashMap<(String, String), Counters>,
    /// Keyed `(tenant, session_id)`, same rule as `by_session_key`.
    pub(crate) sessions: HashMap<(String, String), SessionCounters>,
    /// FNV-1a hashes of every (trace_id, span_id) in the rollup: the
    /// supersede prefilter. A key replaced in a NEWER source makes this
    /// rollup unusable as-is (its counters include the stale version).
    pub(crate) key_hashes: HashSet<u64>,
    /// `$payload` references held by any span in the rollup — the live set
    /// that protects payload files from the TTL sweep.
    pub(crate) payload_refs: HashSet<String>,
}

impl SegmentRollup {
    /// An empty rollup over a known timestamp range: the starting point the
    /// sidecar decoder fills in, so that `min`/`max` are never left at the
    /// `build` sentinel when no span was absorbed.
    pub(crate) fn empty(bounds: crate::rollup_file::Bounds) -> Self {
        Self {
            min_start_ns: bounds.min_start_ns,
            max_start_ns: bounds.max_start_ns,
            min_end_ns: bounds.min_end_ns,
            max_end_ns: bounds.max_end_ns,
            ..Self::default()
        }
    }

    /// The timestamp ranges this rollup covers.
    pub(crate) fn bounds(&self) -> crate::rollup_file::Bounds {
        crate::rollup_file::Bounds {
            min_start_ns: self.min_start_ns,
            max_start_ns: self.max_start_ns,
            min_end_ns: self.min_end_ns,
            max_end_ns: self.max_end_ns,
        }
    }

    /// Folds `spans` into a rollup, deriving cost at `pricing` for the calls
    /// that did not meter one.
    ///
    /// The table is an input to the values this produces, which is why the
    /// sidecar records its fingerprint: a rollup built under one table must
    /// not be believed by a store running another.
    pub(crate) fn build<S: std::borrow::Borrow<Span>>(
        spans: &[S],
        pricing: &crate::pricing::Pricing,
    ) -> Self {
        let mut rollup = Self {
            min_start_ns: u64::MAX,
            min_end_ns: u64::MAX,
            ..Self::default()
        };
        for span in spans {
            rollup.absorb(span.borrow(), pricing);
        }
        if rollup.min_start_ns == u64::MAX {
            rollup.min_start_ns = 0;
        }
        if rollup.min_end_ns == u64::MAX {
            rollup.min_end_ns = 0;
        }
        rollup
    }

    fn absorb(&mut self, span: &Span, pricing: &crate::pricing::Pricing) {
        self.min_start_ns = self.min_start_ns.min(span.start_time_ns);
        self.max_start_ns = self.max_start_ns.max(span.start_time_ns);
        self.min_end_ns = self.min_end_ns.min(span.end_time_ns);
        self.max_end_ns = self.max_end_ns.max(span.end_time_ns);
        self.key_hashes
            .insert(key_hash(&span.tenant, &span.trace_id, &span.span_id));
        crate::erasure::payload_refs_of(span, &mut self.payload_refs);
        // One semconv scan, shared across every group this span joins.
        let facts = semconv::facts(&span.attributes).priced(pricing);
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
                .entry((span.tenant.clone(), session.clone()))
                .or_default()
                .absorb(span, &facts);
            self.sessions
                .entry((span.tenant.clone(), session.clone()))
                .or_default()
                .absorb(span, &facts);
        }
    }
}

// ------------------------------------------------------------ span helpers

/// FNV-1a over the primary key — tenant, trace, span, NUL-separated. Used
/// only as a PREFILTER: a hash collision can force an unnecessary exact
/// re-scan, never a wrong count. The tenant is always hashed, empty included:
/// special-casing the default tenant would save nothing and make the domain
/// of the hash depend on the value being hashed. Persisted key-hash sets are
/// invalidated by the rollup sidecar's SCHEMA_VERSION, which was bumped for
/// exactly this change.
pub(crate) fn key_hash(tenant: &str, trace_id: &str, span_id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in tenant
        .as_bytes()
        .iter()
        .chain([0_u8].iter())
        .chain(trace_id.as_bytes())
        .chain([0_u8].iter())
        .chain(span_id.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
        self.sessions_in(None, since_ns, until_ns, limit, order)
    }

    /// [`Self::sessions`] under a tenant scope: `None` lists every tenant's
    /// sessions (the operator view, each row carrying its tenant), `Some(t)`
    /// only tenant `t`'s. Sessions stay on the rollup fast path either way —
    /// the session maps are keyed by `(tenant, session_id)`, so scoping is a
    /// post-filter on structurally separate rows, never a subtraction.
    pub fn sessions_in(
        &self,
        tenant: Option<&str>,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
        limit: usize,
        order: SessionOrder,
    ) -> Result<Vec<SessionSummary>> {
        let mut merged: HashMap<(String, String), SessionCounters> = HashMap::new();
        self.fold_analytics(since_ns, until_ns, None, |rollup| {
            for (key, counters) in &rollup.sessions {
                if tenant.is_some_and(|tenant| key.0 != tenant) {
                    continue;
                }
                merged.entry(key.clone()).or_default().merge(counters);
            }
        })?;
        let mut sessions: Vec<SessionSummary> = merged
            .into_iter()
            .map(|((tenant, session_id), entry)| SessionSummary {
                session_id,
                tenant,
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
                cost_derived_usd: entry.counters.cost_derived_usd,
                cost_metered_calls: entry.counters.cost_metered_calls,
                cost_derived_calls: entry.counters.cost_derived_calls,
                cost_unpriced_calls: entry.counters.cost_unpriced_calls,
                error_count: entry.counters.errors,
            })
            .collect();
        // The (tenant, session id) pair breaks every tie, so a page is
        // deterministic even when a hundred sessions report the same cost.
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
            ranked
                .then_with(|| a.tenant.cmp(&b.tenant))
                .then_with(|| a.session_id.cmp(&b.session_id))
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
    pub(crate) fn resolve_session_spans(
        &self,
        tenant: Option<&str>,
        session_id: &str,
    ) -> Result<Vec<Span>> {
        let candidates =
            self.query_attribute_union(&semconv::SESSION_KEYS, &session_values(session_id))?;
        Ok(narrow_to_session(candidates, tenant, session_id))
    }

    /// One session with its per-trace breakdown, or `None` when no span
    /// carries the id.
    pub fn session(&self, session_id: &str) -> Result<Option<SessionDetail>> {
        self.session_in(None, session_id)
    }

    /// [`Self::session`] under a tenant scope. `None` resolves the id across
    /// every tenant — unchanged for a single-tenant store, and an operator
    /// drilling into a multi-tenant store should name the tenant, because a
    /// union across tenants that reuse an id is a merged view, reported with
    /// an empty tenant field when the members disagree.
    pub fn session_in(
        &self,
        tenant: Option<&str>,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        let spans = self.resolve_session_spans(tenant, session_id)?;
        if spans.is_empty() {
            return Ok(None);
        }
        // The tenant reported back: the scope when one was given, else the
        // members' unanimous tenant, else empty (a disagreeing union).
        let reported_tenant = match tenant {
            Some(tenant) => tenant.to_owned(),
            None => {
                let first = spans[0].tenant.as_str();
                if spans.iter().all(|span| span.tenant == first) {
                    first.to_owned()
                } else {
                    String::new()
                }
            }
        };
        let mut session = SessionCounters::default();
        let mut traces: BTreeMap<String, (Vec<&Span>, Counters)> = BTreeMap::new();
        for span in &spans {
            let facts = self.facts(span);
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
                    cost_derived_usd: counters.cost_derived_usd,
                    cost_metered_calls: counters.cost_metered_calls,
                    cost_derived_calls: counters.cost_derived_calls,
                    cost_unpriced_calls: counters.cost_unpriced_calls,
                    error_count: counters.errors,
                }
            })
            .collect();
        trace_rows.sort_by_key(|row| row.first_start_ns);
        Ok(Some(SessionDetail {
            summary: SessionSummary {
                session_id: session_id.to_owned(),
                tenant: reported_tenant,
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
                cost_derived_usd: session.counters.cost_derived_usd,
                cost_metered_calls: session.counters.cost_metered_calls,
                cost_derived_calls: session.counters.cost_derived_calls,
                cost_unpriced_calls: session.counters.cost_unpriced_calls,
                error_count: session.counters.errors,
            },
            traces: trace_rows,
        }))
    }

    /// Diagnoses one session: how it ended, which step is to blame, and the
    /// repeated shapes behind that.
    ///
    /// The entire answer is computed from ONE resolution of the session's
    /// spans. That is not an optimization, it is what makes the document
    /// internally consistent: asking the store separately for an outcome, a
    /// repetition report and a failure aggregate lets a seal land between the
    /// calls and produces an answer whose own paragraphs disagree about how
    /// many times a step ran. Invariant 6 states the general form of this —
    /// anything that reads in more than one step has to pin its view.
    ///
    /// `max_spans` bounds the analysis. When it binds, the spans kept are the
    /// EARLIEST by start time, because attribution names the first step that
    /// failed and a suffix would systematically discard the answer.
    pub fn diagnose_session(
        &self,
        tenant: Option<&str>,
        session_id: &str,
        now_ns: u64,
        idle_ns: u64,
        max_spans: usize,
    ) -> Result<Option<crate::attribution::Diagnosis>> {
        let mut spans = self.resolve_session_spans(tenant, session_id)?;
        if spans.is_empty() {
            return Ok(None);
        }
        spans.sort_by(|left, right| {
            left.start_time_ns
                .cmp(&right.start_time_ns)
                .then_with(|| left.trace_id.cmp(&right.trace_id))
                .then_with(|| left.span_id.cmp(&right.span_id))
        });
        Ok(Some(crate::attribution::diagnose(
            &spans, now_ns, idle_ns, max_spans,
        )))
    }

    /// The failing steps of a session, as dataset examples.
    ///
    /// The selection is made HERE, from the server's own diagnosis, and not
    /// from a list of span ids a caller supplied. That is a security property
    /// rather than a convenience: the diagnosis is rendered to a model inside
    /// a block of attacker-controlled telemetry, and a model that reads span
    /// ids out of that block and hands them back would let injected text
    /// choose what gets copied into a durable dataset — one that a later span
    /// or trace erasure does not remove and whose payload references pin
    /// those bytes past retention. Naming the SESSION and re-deriving the
    /// steps makes the promoted set a pure function of the run the operator
    /// named.
    ///
    /// Each example carries its own copy of the failing span's input, so
    /// erasing the source trace afterwards cannot corrupt the dataset, and its
    /// id is derived from the span's address so re-promoting the same session
    /// produces the identical version rather than a second one.
    pub fn promotable_examples(
        &self,
        tenant: Option<&str>,
        session_id: &str,
        diagnosis: &crate::attribution::Diagnosis,
        limit: usize,
    ) -> Result<Vec<crate::evals::NewExample>> {
        // Every span the diagnosis is willing to stand behind: the attributed
        // cause, then the evidence of each finding it called a fault.
        let mut wanted: Vec<(&str, &str)> = Vec::new();
        if let Some(cause) = &diagnosis.cause {
            wanted.push((&cause.span.trace_id, &cause.span.span_id));
        }
        for finding in diagnosis.findings.iter().filter(|f| f.shape.is_fault()) {
            for span in &finding.spans {
                wanted.push((&span.trace_id, &span.span_id));
            }
        }
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }

        let spans = self.resolve_session_spans(tenant, session_id)?;
        let mut examples = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (trace_id, span_id) in wanted {
            if examples.len() >= limit {
                break;
            }
            if !seen.insert((trace_id.to_owned(), span_id.to_owned())) {
                continue;
            }
            let Some(span) = spans
                .iter()
                .find(|span| span.trace_id == trace_id && span.span_id == span_id)
            else {
                continue;
            };
            examples.push(crate::evals::NewExample {
                // Stable across promotions of the same step, so a repeated
                // promote is idempotent at the version's content address.
                example_id: format!(
                    "span-{}",
                    &crate::payload::sha256_hex(format!("{trace_id}/{span_id}").as_bytes())[..16]
                ),
                input: serde_json::json!({
                    "name": span.name,
                    "service": span.service,
                    "status": span.status,
                    "attributes": span.attributes,
                }),
                expected: None,
                split: "regression".to_owned(),
                provenance: Some(crate::evals::Provenance {
                    tenant: span.tenant.clone(),
                    trace_id: span.trace_id.clone(),
                    span_id: span.span_id.clone(),
                }),
            });
        }
        Ok(examples)
    }

    /// Aggregates LLM usage over the window, grouped by `group_by`, sorted by
    /// cost (then tokens, then key).
    pub fn llm_aggregate(
        &self,
        group_by: LlmGroupBy,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
    ) -> Result<Vec<LlmAggregateRow>> {
        self.llm_aggregate_in(None, group_by, since_ns, until_ns)
    }

    /// [`Self::llm_aggregate`] under a tenant scope.
    ///
    /// An unscoped call keeps the rollup fast path. A scoped one cannot for
    /// the model/provider/service/day dimensions — those rollup maps are
    /// store-global and a rollup cannot be subtracted from — so it takes the
    /// exact fold with the tenant predicate applied span by span, the same
    /// slower-but-right trade the pending-erasure mask already makes.
    /// Session rows ARE tenant-keyed, so the session dimension stays on the
    /// fast path and post-filters.
    pub fn llm_aggregate_in(
        &self,
        tenant: Option<&str>,
        group_by: LlmGroupBy,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
    ) -> Result<Vec<LlmAggregateRow>> {
        let fold_scope = match group_by {
            LlmGroupBy::Session => None,
            _ => tenant,
        };
        // Session rows merge STRUCTURALLY by (tenant, session) and only then
        // render a display key. Merging by the rendered string would fuse a
        // default-tenant session literally named "acme/chat" with tenant
        // acme's session "chat" — the exact collision the sidecar's
        // two-string encoding exists to prevent. Two rows may still RENDER
        // alike; equal text on distinct rows is honest, merged counters are
        // not.
        let mut merged: HashMap<(String, String), Counters> = HashMap::new();
        self.fold_analytics(since_ns, until_ns, fold_scope, |rollup| match group_by {
            LlmGroupBy::Session => {
                for (key, counters) in &rollup.by_session_key {
                    if tenant.is_some_and(|tenant| key.0 != tenant) {
                        continue;
                    }
                    merged.entry(key.clone()).or_default().merge(counters);
                }
            }
            _ => {
                let groups: Box<dyn Iterator<Item = (&String, &Counters)>> = match group_by {
                    LlmGroupBy::Model => Box::new(rollup.by_model.iter()),
                    LlmGroupBy::Provider => Box::new(rollup.by_provider.iter()),
                    LlmGroupBy::Service => Box::new(rollup.by_service.iter()),
                    LlmGroupBy::Day => Box::new(rollup.by_day.iter()),
                    LlmGroupBy::Session => unreachable!("handled above"),
                };
                for (key, counters) in groups {
                    merged
                        .entry((String::new(), key.clone()))
                        .or_default()
                        .merge(counters);
                }
            }
        })?;
        let mut rows: Vec<LlmAggregateRow> = merged
            .into_iter()
            .map(|((row_tenant, key), counters)| LlmAggregateRow {
                key: match row_tenant.is_empty() {
                    true => key,
                    false => format!("{row_tenant}/{key}"),
                },
                spans: counters.spans,
                llm_calls: counters.llm_calls,
                prompt_tokens: counters.prompt_tokens,
                completion_tokens: counters.completion_tokens,
                total_tokens: counters.total_tokens,
                cost_usd: counters.cost_usd,
                cost_derived_usd: counters.cost_derived_usd,
                cost_metered_calls: counters.cost_metered_calls,
                cost_derived_calls: counters.cost_derived_calls,
                cost_unpriced_calls: counters.cost_unpriced_calls,
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
    /// `tenant` scopes the fold to one tenant's spans. Like a pending
    /// erasure's mask, a tenant scope declines the rollup fast path outright
    /// — a rollup's global dimensions cannot be subtracted from — and applies
    /// the predicate span by span on the exact path.
    fn fold_analytics(
        &self,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
        tenant: Option<&str>,
        mut visit: impl FnMut(&SegmentRollup),
    ) -> Result<()> {
        let folding = std::time::Instant::now();
        // A pending erasure must not be counted, and a rollup cannot subtract:
        // while one is pending the fast path below is declined outright and
        // every overlapping segment takes the exact path, where the mask
        // applies span by span. The window is the seconds inside one erase
        // call — or, after a crash, until the resumed purge settles — and
        // slower-but-right is the only acceptable trade for that window.
        let mask = self.erasure_mask();
        // Lock order: writer before segments (see Store field docs).
        //
        // Nothing is deep-copied under the writer lock. The buffer holds
        // `Arc<Span>` precisely so a reader can take the whole thing away for
        // the price of a pointer per span, and both the window filter and the
        // key set are derived AFTER the lock is released. Cloning the spans
        // and the key index in here — a `String` pair per buffered span —
        // was the same copy the seal path was restructured to stop paying,
        // reintroduced on the read side.
        let writer = self.lock_writer()?;
        let buffer: Vec<Arc<Span>> = writer.spans.clone();
        drop(writer);

        // ALL buffer keys supersede segment copies, in-window or not, so this
        // is over the whole buffer rather than the window's slice of it.
        let buffer_keys: HashSet<(&str, &str, &str)> = buffer
            .iter()
            .map(|span| {
                (
                    span.tenant.as_str(),
                    span.trace_id.as_str(),
                    span.span_id.as_str(),
                )
            })
            .collect();
        let buffered: Vec<Arc<Span>> = buffer
            .iter()
            .filter(|span| {
                in_window(span.start_time_ns, since_ns, until_ns)
                    && mask.as_deref().map_or(true, |mask| !mask.covers(span))
                    && tenant.map_or(true, |tenant| span.tenant == tenant)
            })
            .map(Arc::clone)
            .collect();
        if !buffered.is_empty() {
            visit(&SegmentRollup::build(&buffered, self.pricing()));
        }
        // Buffer and newer-segment hashes are tracked separately because the
        // shadow latch must tell them apart: a merge retires segment-versus-
        // segment shadowing, while buffer-caused shadowing re-forms as long
        // as a client keeps updating the key, so latching on it would either
        // churn a rewrite per pass or run passes that cannot help. The gate
        // and the exact-path prefilter still test the union.
        let buffer_hashes: HashSet<u64> = buffer_keys
            .iter()
            .map(|(tenant, trace_id, span_id)| key_hash(tenant, trace_id, span_id))
            .collect();
        let mut segment_hashes: HashSet<u64> = HashSet::new();

        // PINNED, not held. The fold below decodes segments and reads sidecar
        // files, and doing that under the segments lock made every analytics
        // request a stall for every writer: a seal takes the writer lock and
        // then this one, so a fold that held it for its whole duration held
        // ingest for its whole duration too. An `Arc<Segment>` keeps its own
        // file descriptor, so a pinned segment stays readable even if a merge
        // or expiry unlinks it while this runs — which is the same guarantee
        // `expire_before_locked` already pins on.
        let segments = self.pin_segments()?;
        // Newest first: paths are zero-padded, so path order is flush order.
        for (position, segment) in segments.iter().enumerate().rev() {
            let rollup = self.segment_rollup(segment)?;
            let overlaps = since_ns.map_or(true, |bound| rollup.max_start_ns >= bound)
                && until_ns.map_or(true, |bound| rollup.min_start_ns <= bound);
            if !overlaps {
                segment_hashes.extend(rollup.key_hashes.iter().copied());
                continue;
            }
            let fully_inside = in_window(rollup.min_start_ns, since_ns, until_ns)
                && in_window(rollup.max_start_ns, since_ns, until_ns);
            // One walk answers both questions: does the gate fail at all, and
            // is any of it segment-caused? The walk ends as soon as both are
            // settled — a segment collision settles them together, so the
            // added cost against plain `.any` is confined to the buffer-only
            // case, where it is the price of not latching a pass that could
            // not have helped.
            let mut shadowed_by_buffer = false;
            let mut shadowed_by_segment = false;
            for hash in rollup.key_hashes.iter() {
                shadowed_by_segment = shadowed_by_segment || segment_hashes.contains(hash);
                if shadowed_by_segment {
                    break;
                }
                shadowed_by_buffer = shadowed_by_buffer || buffer_hashes.contains(hash);
            }
            let possibly_superseded = shadowed_by_buffer || shadowed_by_segment;
            if fully_inside && !possibly_superseded && mask.is_none() && tenant.is_none() {
                visit(&rollup);
                segment_hashes.extend(rollup.key_hashes.iter().copied());
                continue;
            }
            // Only what maintenance can fix latches: a merge retires
            // segment-versus-segment shadowing, while window geometry and
            // buffer-caused shadowing it cannot touch. The observation is
            // free — the gate just computed it.
            if shadowed_by_segment {
                self.note_shadowing();
            }
            // Exact path: decode the window's slice of this segment, dropping
            // spans whose key exists in the buffer or a newer segment.
            //
            // Only the records the window covers are decoded. Segments store
            // records in ascending start-time order, so the window is a
            // contiguous ordinal range; the old full decode read the whole
            // segment to throw most of it away, which is what made a narrow
            // dashboard window cost more than a whole-corpus one.
            let mut survivors: Vec<Span> = Vec::new();
            for span in segment.spans_parsed_in_window(since_ns, until_ns)? {
                if mask.as_deref().is_some_and(|mask| mask.covers(&span)) {
                    continue;
                }
                if tenant.is_some_and(|tenant| span.tenant != tenant) {
                    continue;
                }
                if buffer_keys.contains(&(
                    span.tenant.as_str(),
                    span.trace_id.as_str(),
                    span.span_id.as_str(),
                )) {
                    continue;
                }
                // Gated per newer segment on that segment's OWN hash set —
                // [`crate::superseded_by_newer`], the same test the query
                // paths run. A miss in every set is proof this span was never
                // replaced, and only the specific segments whose set holds
                // the hash — a real supersede or an FNV collision — pay an
                // exact probe, so a key rewritten eleven segments later costs
                // one probe, not a walk across the ten in between. The union
                // in `segment_hashes` stays what it is: the per-ROLLUP gate
                // above, not the per-span one. Without any prefilter every
                // surviving span probed the trace index of every newer
                // segment, so the cost was spans × segments: at eight
                // concurrent ingest clients, where interleaved time ranges
                // leave no segment fully inside a window, that quadratic term
                // was the whole query.
                if !crate::superseded_by_newer(
                    &segments,
                    position,
                    &span,
                    &self.metrics,
                    self.pricing(),
                )? {
                    survivors.push(span);
                }
            }
            if !survivors.is_empty() {
                visit(&SegmentRollup::build(&survivors, self.pricing()));
            }
            segment_hashes.extend(rollup.key_hashes.iter().copied());
        }
        // Superseded segments drop out of the cache with their paths.
        //
        // Against a FRESH read of the segment list, not against the pinned
        // snapshot above: a merge that published while this fold ran installed
        // its output's rollup as it did so, and retaining against a stale
        // snapshot would evict exactly that entry and undo the handover. Both
        // locks are taken here — segments then rollups, the established order
        // — so the liveness test and the eviction cannot disagree. The work is
        // proportional to the cache, not to the corpus.
        {
            let live = self.lock_segments()?;
            let live_paths: HashSet<&PathBuf> = live.iter().map(|segment| &segment.path).collect();
            self.rollups
                .lock()
                .map_err(|_| crate::Error::LockPoisoned("rollups"))?
                .retain(|path, _| live_paths.contains(path));
        }
        self.metrics
            .analytics_fold
            .record(u64::try_from(folding.elapsed().as_nanos()).unwrap_or(u64::MAX));
        Ok(())
    }

    /// Every `$payload` reference held by a live span (buffer + all
    /// segments): the protection set for the TTL payload sweep. Superseded
    /// span versions may contribute references; that over-protection only
    /// delays deletion, never loses data.
    pub(crate) fn live_payload_refs(&self) -> Result<HashSet<String>> {
        let mut refs = HashSet::new();
        // Pointer copies under the lock; the scan happens after it is dropped.
        let buffer: Vec<Arc<Span>> = {
            let writer = self.lock_writer()?;
            writer.spans.clone()
        };
        for span in &buffer {
            crate::erasure::payload_refs_of(span, &mut refs);
        }
        // Dataset examples are live references too: an example deliberately
        // outlives the trace it was promoted from, and "deleting source
        // traces must not corrupt dataset versions" is only true for
        // offloaded content if this union says so — to the TTL sweep and to
        // reference-aware erasure alike.
        refs.extend(self.eval_payload_refs()?);
        // Pinned, not held: this runs from the maintenance thread once a
        // minute and reads a file per segment, and holding the segments lock
        // across that stalled every seal for the duration.
        for segment in self.pin_segments()? {
            // Read the sidecar WITHOUT caching it. The payload sweep asks
            // about every segment in the corpus, including ones no
            // aggregation has ever touched, so routing it through the cache
            // made a timer — not a query — the thing that decided how much
            // memory the rollup cache held. A miss falls back to the caching
            // path, because at that point the segment has to be decoded
            // anyway and the result is worth keeping.
            let binding = segment.rollup_binding(self.pricing_fingerprint());
            // Warm cache first — bypassing it made every tick re-read and
            // re-decode sidecars the process already had in memory. Then the
            // sidecar, read WITHOUT caching: the sweep asks about every
            // segment in the corpus, including ones no aggregation has ever
            // touched, and routing that through the cache made a timer rather
            // than a query decide how much memory the cache held. Only a real
            // miss falls back to the caching path, because at that point the
            // segment has to be decoded anyway and the result is worth
            // keeping.
            if let Some(rollup) = self.cached_rollup(&segment.path, binding)? {
                refs.extend(rollup.payload_refs.iter().cloned());
            } else if let Some(rollup) = crate::rollup_file::load(&segment.path, binding) {
                refs.extend(rollup.payload_refs);
            } else {
                refs.extend(self.segment_rollup(&segment)?.payload_refs.iter().cloned());
            }
        }
        Ok(refs)
    }

    /// The segment's rollup: from the in-memory cache, else from the segment's
    /// on-disk sidecar, else built by decoding the segment.
    ///
    /// The middle step is what a restart lives on. The in-memory cache is
    /// empty in a fresh process, so without a sidecar the first aggregation
    /// after every restart decodes the entire corpus — seconds, scaling with
    /// bytes on disk. The sidecar turns that into a read of a file roughly
    /// proportional to the segment's DISTINCT keys plus eight bytes per span.
    ///
    /// A miss on the sidecar writes one, so a store that predates this code
    /// (or lost one to a crash) heals on its first query rather than paying
    /// the decode on every restart forever. The write is best-effort: failing
    /// a query because a derived cache could not be saved would trade a
    /// correct slow answer for no answer.
    fn segment_rollup(&self, segment: &crate::Segment) -> Result<Arc<SegmentRollup>> {
        let binding = segment.rollup_binding(self.pricing_fingerprint());
        if let Some(rollup) = self.cached_rollup(&segment.path, binding)? {
            return Ok(rollup);
        }
        let rollup = match crate::rollup_file::load(&segment.path, binding) {
            Some(rollup) => Arc::new(rollup),
            None => {
                let rollup = Arc::new(SegmentRollup::build(
                    &segment.spans_parsed()?,
                    self.pricing(),
                ));
                let _ = crate::rollup_file::store(&segment.path, binding, &rollup);
                rollup
            }
        };
        // Last writer wins, including over an entry for a different
        // generation of this path. That is safe because reads check the
        // binding: an entry this overwrote is either the one we would have
        // written anyway, or a stale one, or a newer one that the next reader
        // rejects and reloads from its sidecar. Correct either way, and
        // self-healing; the only cost of losing this race is one extra
        // sidecar read.
        self.rollups
            .lock()
            .map_err(|_| crate::Error::LockPoisoned("rollups"))?
            .insert(segment.path.clone(), (binding, Arc::clone(&rollup)));
        Ok(rollup)
    }

    /// The tail suffix of segments that provably share keys, as paths, or
    /// `None` when no collision is visible within `budget_bytes` of the tail.
    ///
    /// This is [`Store::compact_shadowed`]'s scan. It walks newest-to-oldest
    /// so "shares keys" means "with a NEWER segment" — the direction
    /// last-write-wins arbitrates and the fold's gate tests. It reads only
    /// rollups already in cache or on disk as sidecars: a segment with
    /// neither ends the scan rather than being decoded, because maintenance
    /// that decodes segments to decide whether to merge them has already paid
    /// most of what the merge would. Freshly sealed segments carry sidecars,
    /// so the segment that ends a scan is a pre-sidecar survivor that the
    /// first aggregation to touch it will heal, after which the scan sees
    /// through it.
    ///
    /// The byte budget keeps both the walk's transient hash set and any merge
    /// chosen from it bounded by configuration rather than by corpus size.
    pub(crate) fn shadowed_tail_suffix(
        &self,
        budget_bytes: u64,
    ) -> crate::Result<Option<Vec<PathBuf>>> {
        let segments = self.pin_segments()?;
        let mut seen: HashSet<u64> = HashSet::new();
        let mut bytes = 0u64;
        let mut start: Option<usize> = None;
        for (position, segment) in segments.iter().enumerate().rev() {
            bytes = bytes.saturating_add(segment.bytes);
            if bytes > budget_bytes {
                break;
            }
            let binding = segment.rollup_binding(self.pricing_fingerprint());
            let rollup = match self.cached_rollup(&segment.path, binding)? {
                Some(rollup) => rollup,
                None => match crate::rollup_file::load(&segment.path, binding) {
                    Some(rollup) => Arc::new(rollup),
                    None => break,
                },
            };
            if rollup.key_hashes.iter().any(|hash| seen.contains(hash)) {
                start = Some(position);
            }
            seen.extend(rollup.key_hashes.iter().copied());
        }
        Ok(start.map(|position| {
            segments[position..]
                .iter()
                .map(|segment| segment.path.clone())
                .collect()
        }))
    }

    /// The cached rollup for `path`, but only if it was built from the segment
    /// `binding` identifies.
    ///
    /// The check is the whole point. A path is not an identity — TTL expiry
    /// rewrites a segment in place — and a reader pins the segment list rather
    /// than holding the lock, so the caller may be holding a descriptor to one
    /// generation of a segment while the cache holds the rollup for another.
    /// Serving that hit mixes the two, and the fold's supersede prefilter
    /// turns the mismatch into a double-counted span. A rejected hit costs a
    /// sidecar read, which is what an empty cache would have cost anyway.
    fn cached_rollup(
        &self,
        path: &PathBuf,
        binding: crate::rollup_file::Binding,
    ) -> Result<Option<Arc<SegmentRollup>>> {
        let cache = self
            .rollups
            .lock()
            .map_err(|_| crate::Error::LockPoisoned("rollups"))?;
        Ok(cache
            .get(path)
            .and_then(|(cached, rollup)| (*cached == binding).then(|| Arc::clone(rollup))))
    }
}

/// [`Store::resolve_session_spans`] against a buffer and segment set the
/// caller is holding still — the pinned-view half of the same resolution, used
/// by [`crate::SnapshotView`] so a paged export can filter by session.
pub(crate) fn resolve_session_spans_in(
    buffer: &crate::WriteBuffer,
    segments: &[Arc<crate::Segment>],
    tenant: Option<&str>,
    session_id: &str,
    mask: Option<&crate::erasure::Mask>,
) -> Result<Vec<Span>> {
    let candidates = crate::attribute_union_view(
        buffer,
        segments,
        &semconv::SESSION_KEYS,
        &session_values(session_id),
        mask,
    )?;
    Ok(narrow_to_session(candidates, tenant, session_id))
}

/// Every JSON encoding that normalizes to the session id `session_id`.
///
/// Session normalization stringifies numeric attributes, so a producer that
/// sent `"gen_ai.conversation.id": 4711` yields the session id "4711".
/// Matching only the JSON string would then list that session and refuse to
/// open it.
pub(crate) fn session_values(session_id: &str) -> Vec<Value> {
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

/// The attribute union over-selects two ways: a span may carry a matching
/// value under a LOWER-precedence key while its resolved session is something
/// else, and the value index is tenant-blind, so another tenant's span under
/// the same session id is a candidate too. Both are settled here, against the
/// decoded span.
fn narrow_to_session(candidates: Vec<Span>, tenant: Option<&str>, session_id: &str) -> Vec<Span> {
    candidates
        .into_iter()
        .filter(|span| tenant.map_or(true, |tenant| span.tenant == tenant))
        .filter(|span| semconv::facts(&span.attributes).session.as_deref() == Some(session_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup_file::Binding;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "traza-rollup-cache-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// A cache entry is keyed by path but ANSWERS FOR AN IDENTITY.
    ///
    /// This is the check that makes pinning the segment list safe. A path is
    /// not an identity — TTL expiry rewrites a segment in place — and a fold
    /// pins the segment list rather than holding the lock, so it can be
    /// decoding one generation of a segment through its own descriptor while
    /// the cache already holds the rollup for the rewritten bytes. Serving
    /// that hit mixes the two: the survivors come from the old bytes and the
    /// supersede prefilter from the new rollup, so a key both generations
    /// still contain slips past the prefilter and is counted twice.
    ///
    /// The interleaving that produces it is a genuine race and an unreliable
    /// thing to reproduce from outside, so the invariant is pinned here
    /// instead, where it is exact: the same path with a different binding is a
    /// MISS, and every field of the binding is load-bearing.
    #[test]
    fn a_cached_rollup_answers_only_for_the_segment_it_was_built_from() {
        let dir = temp_dir("identity");
        let store = Store::open(&dir, crate::Config::default()).expect("opens");
        let path = dir.join("segment-00000000000000000000.seg");
        let binding = Binding {
            segment_bytes: 4_096,
            record_count: 10,
            min_start_ns: 1_000,
            max_start_ns: 9_000,
            pricing_fingerprint: 0,
        };
        let rollup = Arc::new(SegmentRollup::default());
        store
            .rollups
            .lock()
            .expect("rollups")
            .insert(path.clone(), (binding, Arc::clone(&rollup)));

        assert!(
            store
                .cached_rollup(&path, binding)
                .expect("lookup")
                .is_some(),
            "the identical binding must hit"
        );

        // Every field is part of the identity. A rewritten segment differs in
        // its byte length and record count; a re-sealed one can differ only in
        // its timestamp range. None of them may be ignored.
        for (label, other) in [
            (
                "bytes",
                Binding {
                    segment_bytes: 4_097,
                    ..binding
                },
            ),
            (
                "record count",
                Binding {
                    record_count: 9,
                    ..binding
                },
            ),
            (
                "min start",
                Binding {
                    min_start_ns: 1_001,
                    ..binding
                },
            ),
            (
                "max start",
                Binding {
                    max_start_ns: 9_001,
                    ..binding
                },
            ),
            (
                // Counters folded at one set of rates are not the counters
                // another set would have produced, and the segment they came
                // from is byte-identical either way — so nothing else in the
                // binding can catch this.
                "pricing",
                Binding {
                    pricing_fingerprint: 0xdead_beef,
                    ..binding
                },
            ),
        ] {
            assert!(
                store.cached_rollup(&path, other).expect("lookup").is_none(),
                "a rollup built from a segment with a different {label} must \
                 not be served for this one"
            );
        }

        // An unknown path is a miss rather than a panic.
        assert!(store
            .cached_rollup(&dir.join("segment-00000000000000000001.seg"), binding)
            .expect("lookup")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
