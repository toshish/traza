//! Live run state: what one run has used, folded from its spans, and what
//! its limits make of that.
//!
//! The fold is the same whether spans arrive one acknowledged batch at a
//! time (live) or all at once from the store (a rebuild), so the two paths
//! cannot drift apart in what they count — or in where a stop latches, which
//! [`RunState::fold`] checks after every span on both, in step order and
//! under the run's policy as the whole fold leaves it. Each span's
//! contribution is kept as a small [`Mark`] under a hash of its id, which is
//! what makes a re-ingested span an UPSERT here as it is in the store: a
//! client retrying a batch the store already acknowledged changes nothing,
//! and a span re-sent with different content replaces its old contribution
//! rather than adding a second one. Each mark also carries the store's write
//! revision of the version it came from, because observers run after their
//! writes in no particular order: a version the store has since replaced
//! must not replace the newer one here when its observer runs last.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::policy::{Action, Limits, Resolved};
use super::Verdict;
use crate::semconv::{self, StepKind};
use crate::{Error, Result, Span, Store};

/// Longest fingerprint rendered into a reason, in characters. Fingerprints
/// carry a tool or model name from the telemetry, and a reason line is not
/// the place for an unbounded one.
const REASON_FINGERPRINT_CHARS: usize = 160;

/// Fraction of a limit at which a run is warned.
const WARN_FRACTION: f64 = 0.8;

/// A span by identity, as evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpanRef {
    /// The span's trace.
    pub trace_id: String,
    /// The span.
    pub span_id: String,
}

/// The run's most-repeated step fingerprint.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopRepeat {
    /// `{kind}:{name}#{request hash}` (see [`semconv::StepFacts`]).
    pub fingerprint: String,
    /// Occurrences since the run's last reset.
    pub count: u64,
    /// The latest occurrence.
    pub example: SpanRef,
}

/// What a run has used.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Usage {
    /// LLM, tool and agent spans.
    pub steps: u64,
    /// LLM steps.
    pub llm_calls: u64,
    /// Tool steps.
    pub tool_calls: u64,
    /// Tokens across every span of the run (a span's reported total, else
    /// prompt plus completion) — the same sum the session analytics report.
    pub tokens: u64,
    /// Priced cost across every span of the run, in USD.
    pub cost_usd: f64,
    /// Spans with status `error`, of any kind.
    pub errors: u64,
    /// Consecutive failed steps at the end of the run, since its last reset.
    pub error_streak: u64,
    /// First span start to last span end, by the spans' own clocks.
    pub duration_ns: u64,
    /// The most-repeated step fingerprint, when any step occurred twice.
    pub top_repeat: Option<TopRepeat>,
}

/// A step a pre-call check asks about: what it is expected to cost.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextStep {
    /// The step's estimated cost, in USD.
    #[serde(default)]
    pub est_cost_usd: Option<f64>,
    /// The step's estimated tokens.
    #[serde(default)]
    pub est_tokens: Option<u64>,
    /// The tool the step calls; absent, the step is counted as an LLM call.
    #[serde(default)]
    pub tool: Option<String>,
}

impl NextStep {
    /// Refuses an estimate that is not a cost.
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(cost) = self.est_cost_usd {
            if !cost.is_finite() || cost < 0.0 {
                return Err(Error::Invalid(format!(
                    "next.est_cost_usd must be a non-negative finite number (got {cost})"
                )));
            }
        }
        Ok(())
    }
}

/// Who stopped a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrippedBy {
    /// A policy limit.
    Policy,
    /// An operator, with the kill switch.
    Manual,
}

/// A sticky stop.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Trip {
    pub(crate) at_ns: u64,
    pub(crate) reasons: Vec<String>,
    pub(crate) evidence: Vec<SpanRef>,
}

/// A span reduced to what a run's usage needs. Extracted with no lock held:
/// a fingerprint includes a request digest, which costs time proportional to
/// the request's size.
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    key: u64,
    trace_id: String,
    span_id: String,
    service: String,
    kind: StepKind,
    error: bool,
    tokens: u64,
    cost_usd: f64,
    fingerprint: Option<String>,
    start_ns: u64,
    pub(crate) end_ns: u64,
    /// The store's write revision of this version of the span (see
    /// `Store::observe_admitted`), or 0 for one a rebuild read from the
    /// store, which is ordered by the rebuilt state's floor instead.
    revision: u64,
}

impl Observation {
    /// Reads one span: priced facts for tokens and cost, the step kind, and
    /// — for steps only — the fingerprint. Non-step spans never pay for a
    /// request digest they would not use, and no span pays for a response
    /// digest: the fingerprint does not cover the response. `revision` is
    /// the write revision the version was stored at; 0 for a stored span a
    /// rebuild read.
    pub(crate) fn of(store: &Store, span: &Span, revision: u64) -> Self {
        let llm = store.facts(span);
        let kind = semconv::step_kind_with(span, &llm);
        let fingerprint = kind
            .is_step()
            .then(|| semconv::step_fingerprint_with(span, &llm));
        Self {
            key: span_key(&span.trace_id, &span.span_id),
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
            service: span.service.clone(),
            kind,
            error: span.status == "error",
            tokens: llm.total(),
            cost_usd: llm.cost_usd.filter(|cost| cost.is_finite()).unwrap_or(0.0),
            fingerprint,
            start_ns: span.start_time_ns,
            end_ns: span.end_time_ns,
            revision,
        }
    }

    /// Whether the span is a step of its run.
    pub(crate) fn is_step(&self) -> bool {
        self.kind.is_step()
    }

    fn span_ref(&self) -> SpanRef {
        SpanRef {
            trace_id: self.trace_id.clone(),
            span_id: self.span_id.clone(),
        }
    }
}

/// A 64-bit key for a span within its run (the tenant is fixed by the run).
/// Length-prefixed so `("ab", "c")` and `("a", "bc")` differ.
pub(crate) fn span_key(trace_id: &str, span_id: &str) -> u64 {
    let mut bytes = Vec::with_capacity(8 + trace_id.len() + span_id.len());
    bytes.extend_from_slice(&(trace_id.len() as u32).to_le_bytes());
    bytes.extend_from_slice(trace_id.as_bytes());
    bytes.extend_from_slice(span_id.as_bytes());
    key_of(&bytes)
}

/// A non-zero 64-bit key for a fingerprint; zero means "no fingerprint".
fn fingerprint_key(fingerprint: &str) -> u64 {
    key_of(fingerprint.as_bytes()).max(1)
}

fn key_of(bytes: &[u8]) -> u64 {
    let digest = crate::hash::hash128(bytes);
    let mut low = [0u8; 8];
    low.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(low)
}

/// One span's contribution, kept so a re-ingest can replace it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Mark {
    kind: StepKind,
    error: bool,
    tokens: u64,
    cost_usd: f64,
    /// [`fingerprint_key`], or 0 for a span with no fingerprint.
    fingerprint: u64,
    /// Absorbed before the run's last reset: it counts toward cumulative
    /// usage but not toward the repeat table or the error streak, which a
    /// reset clears.
    before_reset: bool,
    /// The write revision of the version this mark holds (0: read by a
    /// rebuild). An observation of an older revision is stale.
    revision: u64,
}

impl Mark {
    /// Whether this span counts in the repeat table.
    fn counted(&self) -> bool {
        !self.before_reset && self.kind.is_step() && self.fingerprint != 0
    }

    /// Same contribution, so re-absorbing it would change nothing.
    fn same_as(&self, other: &Mark) -> bool {
        self.kind == other.kind
            && self.error == other.error
            && self.tokens == other.tokens
            && self.cost_usd.to_bits() == other.cost_usd.to_bits()
            && self.fingerprint == other.fingerprint
    }
}

/// A fingerprint's count, with its text and latest example once it repeats.
/// One-off fingerprints — most steps — cost the count alone.
#[derive(Debug)]
struct Repeats {
    count: u64,
    detail: Option<Box<(String, SpanRef)>>,
}

/// One run's folded state.
#[derive(Debug)]
pub(crate) struct RunState {
    /// The service of the earliest-starting span absorbed — the run's first
    /// span, which picks its policy. By start time, not arrival: a root span
    /// ends, and so arrives, after its children, and a rebuild (which reads
    /// spans in start order) must pick the same policy live state did.
    pub(crate) service: Option<String>,
    first_start_ns: u64,
    last_end_ns: u64,
    steps: u64,
    llm_calls: u64,
    tool_calls: u64,
    tokens: u64,
    cost_usd: f64,
    errors: u64,
    error_streak: u64,
    repeats: HashMap<u64, Repeats>,
    top: Option<u64>,
    marks: HashMap<u64, Mark>,
    last_span: Option<SpanRef>,
    /// Whether this state starts from the run's stored history (a rebuild,
    /// or a first query that found nothing stored) rather than from whatever
    /// this process happened to observe.
    pub(crate) complete: bool,
    /// Whether that history was cut short: the run held more spans than a
    /// rebuild reads, so this state starts from its earliest ones only and
    /// what it counts is a lower bound. A stop judged from it is exact; any
    /// other verdict is not an answer.
    pub(crate) truncated: bool,
    /// Writes at or below this revision are already reflected here: the
    /// state was rebuilt from a read of the store exactly as of it, so an
    /// observer of one that runs only now has nothing to add — and its
    /// version may be older than the one the read found. Writes above it
    /// are in no read this state came from, so every one of them counts.
    pub(crate) floor: u64,
    /// A policy stop, sticky until reset.
    pub(crate) trip: Option<Trip>,
    /// The warn level last announced: 0 none, 1 approaching a limit, 2 a
    /// limit reached under a `warn` policy.
    pub(crate) warn_level: u8,
    /// Recency position in the guard's eviction order.
    pub(crate) tick: u64,
    /// The verdict class the guard's listing index files this run under —
    /// kept here so a re-filing knows where to remove it from.
    pub(crate) class: Verdict,
    /// Server clock at the last observation or query, Unix nanoseconds.
    pub(crate) last_seen_ns: u64,
}

impl RunState {
    pub(crate) fn new(complete: bool) -> Self {
        Self {
            service: None,
            first_start_ns: u64::MAX,
            last_end_ns: 0,
            steps: 0,
            llm_calls: 0,
            tool_calls: 0,
            tokens: 0,
            cost_usd: 0.0,
            errors: 0,
            error_streak: 0,
            repeats: HashMap::new(),
            top: None,
            marks: HashMap::new(),
            last_span: None,
            complete,
            truncated: false,
            floor: 0,
            trip: None,
            warn_level: 0,
            tick: 0,
            class: Verdict::Continue,
            last_seen_ns: 0,
        }
    }

    /// Whether any span keyed in `keys` (by [`span_key`]) contributed here.
    pub(crate) fn holds_any(&self, keys: &std::collections::HashSet<u64>) -> bool {
        self.marks.keys().any(|key| keys.contains(key))
    }

    /// Spans this state holds a mark for — its share of the guard's memory
    /// budget.
    pub(crate) fn tracked(&self) -> usize {
        self.marks.len()
    }

    /// Folds one span in, replacing the contribution of an earlier version
    /// of the same span. Returns whether anything changed.
    ///
    /// `before_reset` absorbs a span as history the last reset cleared — a
    /// rebuild's spans that ended before it. A re-ingested span keeps the
    /// side of the reset its first version landed on.
    ///
    /// A version older than the one held — by write revision — changes
    /// nothing: two writers of one span are stored in order, but their
    /// observers run in whichever order they finish, and the guard must end
    /// up holding what the store holds.
    pub(crate) fn absorb(&mut self, observation: &Observation, before_reset: bool) -> bool {
        if self.is_stale(observation) {
            return false;
        }
        let previous = self.marks.get(&observation.key).copied();
        // The span's clocks count whether or not its contribution changed:
        // an upsert that only moved a span's end still moves the run's.
        let clocked = self.clock(observation);
        let mark = Mark {
            kind: observation.kind,
            error: observation.error,
            tokens: observation.tokens,
            cost_usd: observation.cost_usd,
            fingerprint: observation
                .fingerprint
                .as_deref()
                .map_or(0, fingerprint_key),
            before_reset: previous.map_or(before_reset, |previous| previous.before_reset),
            revision: observation.revision,
        };
        if let Some(previous) = previous {
            if previous.same_as(&mark) {
                // A retried batch: the store upserted the same span to the
                // same value, and so does this — not even the streak moves.
                // The revision still advances, so a version written between
                // the two cannot pass for the newer one when it lands late.
                self.marks.insert(observation.key, mark);
                if clocked {
                    self.last_span = Some(observation.span_ref());
                }
                return clocked;
            }
            self.subtract(&previous);
        }
        self.add(&mark, observation);
        if mark.kind.is_step() && !mark.before_reset {
            let already_failed =
                previous.is_some_and(|previous| previous.kind.is_step() && previous.error);
            match (mark.error, already_failed) {
                (true, false) => self.error_streak = self.error_streak.saturating_add(1),
                (true, true) => {}
                (false, _) => self.error_streak = 0,
            }
        }
        self.marks.insert(observation.key, mark);
        true
    }

    fn add(&mut self, mark: &Mark, observation: &Observation) {
        if mark.kind.is_step() {
            self.steps = self.steps.saturating_add(1);
        }
        match mark.kind {
            StepKind::Llm => self.llm_calls = self.llm_calls.saturating_add(1),
            StepKind::Tool => self.tool_calls = self.tool_calls.saturating_add(1),
            _ => {}
        }
        self.tokens = self.tokens.saturating_add(mark.tokens);
        self.cost_usd += mark.cost_usd;
        if mark.error {
            self.errors = self.errors.saturating_add(1);
        }
        if mark.counted() {
            if let Some(fingerprint) = observation.fingerprint.as_deref() {
                self.count_repeat(mark.fingerprint, fingerprint, observation);
            }
        }
        self.last_span = Some(observation.span_ref());
    }

    /// Widens the run's span-clock bounds to cover one span, which takes the
    /// run's policy when it starts earliest. Returns whether either bound
    /// moved.
    ///
    /// Applied to every version of a span, apart from its contribution:
    /// the contribution is deduplicated on what a mark holds, and a mark
    /// holds no clocks. The bounds only ever widen — an upsert that narrowed
    /// a span leaves them slightly wide, which errs toward the limit, where
    /// keeping each span's clocks to narrow them exactly would cost every
    /// mark another 16 bytes.
    fn clock(&mut self, observation: &Observation) -> bool {
        self.take_service(observation);
        let moved =
            observation.start_ns < self.first_start_ns || observation.end_ns > self.last_end_ns;
        self.first_start_ns = self.first_start_ns.min(observation.start_ns);
        self.last_end_ns = self.last_end_ns.max(observation.end_ns);
        moved
    }

    /// Takes `observation`'s service as the run's — which picks its policy —
    /// when it starts before every span absorbed so far, or is the first.
    /// Strictly before: among spans starting together, the first to arrive
    /// keeps the run's policy, so a retry cannot flip it.
    fn take_service(&mut self, observation: &Observation) {
        if self.service.is_none() || observation.start_ns < self.first_start_ns {
            self.service = Some(observation.service.clone());
        }
    }

    /// Whether `observation` is of a version older than one this state
    /// already reflects — at or below the floor a rebuild set, or no newer
    /// than the version of its span held — so that absorbing it must change
    /// nothing. A version without a revision (a rebuild's read) never is.
    fn is_stale(&self, observation: &Observation) -> bool {
        observation.revision != 0
            && (observation.revision <= self.floor
                || self
                    .marks
                    .get(&observation.key)
                    .is_some_and(|mark| observation.revision <= mark.revision))
    }

    /// What a fold absorbs of `observations`: one version per span — the
    /// newest by write revision, which is the one the store kept — minus
    /// any older than what this state already reflects, in step order
    /// (`(start, trace, span)`, the order a replay reads the store in).
    ///
    /// Each version goes where its OWN start puts it: a newer version that
    /// moved a span must not be judged at the older one's place, which the
    /// store no longer has and a replay never sees. Input already in step
    /// order with one version per span — a replay's — comes back as it went
    /// in, in linear time.
    fn in_step_order<'a>(&self, observations: &'a [Observation]) -> Vec<&'a Observation> {
        let mut newest: HashMap<u64, usize> = HashMap::with_capacity(observations.len());
        for (index, observation) in observations.iter().enumerate() {
            if self.is_stale(observation) {
                continue;
            }
            newest
                .entry(observation.key)
                .and_modify(|held| {
                    if observation.revision >= observations[*held].revision {
                        *held = index;
                    }
                })
                .or_insert(index);
        }
        let mut ordered: Vec<&Observation> = observations
            .iter()
            .enumerate()
            .filter(|(index, observation)| newest.get(&observation.key) == Some(index))
            .map(|(_, observation)| observation)
            .collect();
        // Stable, and linear on input that is already sorted.
        ordered.sort_by(|left, right| {
            left.start_ns
                .cmp(&right.start_ns)
                .then_with(|| left.trace_id.cmp(&right.trace_id))
                .then_with(|| left.span_id.cmp(&right.span_id))
        });
        ordered
    }

    /// Folds `observations` in step order, latching a policy stop at the
    /// first span whose absorption reaches a stop limit — checked after
    /// EVERY span, live and in replay alike, so where a run stops depends on
    /// its spans and not on how they were batched: a limit crossed and
    /// cleared again within one batch (an error streak a success then ends)
    /// was crossed, exactly as it is when the same spans arrive one per
    /// batch or are replayed from the store. Returns whether this fold
    /// latched a stop.
    ///
    /// The observations may come in any order and carry several versions of
    /// a span (a batch can, and what arrives during a rebuild spans several
    /// batches): each span is folded once, as the store kept it, where step
    /// order puts it ([`Self::in_step_order`]). And the run's policy is
    /// settled before ANY of them is judged: it is the earliest span's, and
    /// a root agent span, which ends and so arrives after its children,
    /// often comes behind them in the same batch. Judging a child against
    /// its own service's policy on the way to the root would latch a stop
    /// the root's policy never allows and a replay — root first — never
    /// latches.
    ///
    /// `reset_at` absorbs spans that ended at or before it as history the
    /// last reset cleared, which latches nothing (see [`replay`]).
    /// `stop_limits` gives the limits a stop latches at for a run whose
    /// first span had the given service — `None` when nothing can latch (a
    /// `warn` policy, or a kill switch already stopping the run) — and is
    /// asked at most once. `stamp` times a latched stop: the server clock
    /// live, the crossing span's end in a replay, which has no admission
    /// clock to offer.
    pub(crate) fn fold(
        &mut self,
        observations: &[Observation],
        reset_at: Option<u64>,
        stop_limits: &dyn Fn(&str) -> Option<Limits>,
        stamp: &dyn Fn(&Observation) -> u64,
    ) -> bool {
        let observations = self.in_step_order(observations);
        // In step order nothing after the first starts before it, so the
        // service it leaves the run with stands for the whole fold.
        if let Some(first) = observations.first() {
            self.take_service(first);
        }
        let mut resolved: Option<Option<Limits>> = None;
        let mut latched = false;
        for observation in observations {
            let before = reset_at.is_some_and(|reset| observation.end_ns <= reset);
            if !self.absorb(observation, before) || before || self.trip.is_some() {
                continue;
            }
            let limits =
                *resolved.get_or_insert_with(|| stop_limits(self.service.as_deref().unwrap_or("")));
            let Some(limits) = limits else { continue };
            if !reaches(&self.measures(), &limits) {
                continue;
            }
            let assessment = self.assess(&limits, None);
            if !assessment.reached.is_empty() {
                self.trip = Some(Trip {
                    at_ns: stamp(observation),
                    evidence: self.trip_evidence(&assessment),
                    reasons: assessment.reached,
                });
                latched = true;
            }
        }
        latched
    }

    /// Marks this state as rebuilt from only the earliest of the run's
    /// spans. Its counts stand as lower bounds, but the error streak is not
    /// one: it counts the failures at the END of the run, which is exactly
    /// the part that was not read, so it starts again from the spans that
    /// arrive from here.
    pub(crate) fn cut_short(&mut self) {
        self.truncated = true;
        self.error_streak = 0;
    }

    fn subtract(&mut self, mark: &Mark) {
        if mark.kind.is_step() {
            self.steps = self.steps.saturating_sub(1);
        }
        match mark.kind {
            StepKind::Llm => self.llm_calls = self.llm_calls.saturating_sub(1),
            StepKind::Tool => self.tool_calls = self.tool_calls.saturating_sub(1),
            _ => {}
        }
        self.tokens = self.tokens.saturating_sub(mark.tokens);
        // Exactly what was added, so a replaced span leaves no residue
        // beyond float rounding; clamped so rounding cannot go negative.
        self.cost_usd = (self.cost_usd - mark.cost_usd).max(0.0);
        if mark.error {
            self.errors = self.errors.saturating_sub(1);
        }
        if mark.counted() {
            self.uncount_repeat(mark.fingerprint);
        }
    }

    fn count_repeat(&mut self, key: u64, fingerprint: &str, observation: &Observation) {
        let entry = self.repeats.entry(key).or_insert(Repeats {
            count: 0,
            detail: None,
        });
        entry.count = entry.count.saturating_add(1);
        if entry.count >= 2 {
            entry.detail = Some(Box::new((fingerprint.to_owned(), observation.span_ref())));
        }
        let count = entry.count;
        let top_count = self
            .top
            .and_then(|top| self.repeats.get(&top))
            .map_or(0, |top| top.count);
        if self.top != Some(key) && count > top_count {
            self.top = Some(key);
        }
    }

    fn uncount_repeat(&mut self, key: u64) {
        if let Some(entry) = self.repeats.get_mut(&key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 {
                self.repeats.remove(&key);
            } else if entry.count < 2 {
                entry.detail = None;
            }
        }
        if self.top == Some(key) {
            // Rare — only a re-ingest that changed a repeated step's input —
            // so a scan is cheaper than keeping an ordered index for it.
            self.top = self
                .repeats
                .iter()
                .max_by_key(|(_, entry)| entry.count)
                .map(|(key, _)| *key);
        }
    }

    /// Clears what a reset clears: the sticky stop, the warn level, the
    /// error streak and the repeat table. Cumulative usage is untouched — a
    /// budget does not refill because someone pressed reset; a grant is what
    /// raises it.
    pub(crate) fn reset(&mut self) {
        self.trip = None;
        self.warn_level = 0;
        self.error_streak = 0;
        self.repeats.clear();
        self.top = None;
        for mark in self.marks.values_mut() {
            mark.before_reset = true;
        }
    }

    fn top_repeat(&self) -> Option<(&Repeats, &str, &SpanRef)> {
        let entry = self.repeats.get(&self.top?)?;
        let (fingerprint, example) = entry.detail.as_deref()?;
        (entry.count >= 2).then_some((entry, fingerprint.as_str(), example))
    }

    fn measures(&self) -> Measures {
        Measures {
            cost_usd: self.cost_usd,
            tokens: self.tokens,
            steps: self.steps,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            duration_ns: self.duration_ns(),
            repeats: self.top_repeat().map_or(0, |(entry, _, _)| entry.count),
            error_streak: self.error_streak,
        }
    }

    fn duration_ns(&self) -> u64 {
        self.last_end_ns.saturating_sub(self.first_start_ns)
    }

    /// The run's usage, for a verdict document.
    pub(crate) fn usage(&self) -> Usage {
        Usage {
            steps: self.steps,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            tokens: self.tokens,
            cost_usd: self.cost_usd,
            errors: self.errors,
            error_streak: self.error_streak,
            duration_ns: self.duration_ns(),
            top_repeat: self
                .top_repeat()
                .map(|(entry, fingerprint, example)| TopRepeat {
                    fingerprint: fingerprint.to_owned(),
                    count: entry.count,
                    example: example.clone(),
                }),
        }
    }

    /// What `limits` make of this run's usage, plus `next` hypothetically.
    pub(crate) fn assess(&self, limits: &Limits, next: Option<&NextStep>) -> Assessment {
        let findings = assess(&self.measures(), limits, next);
        let top = self
            .top_repeat()
            .map(|(_, fingerprint, example)| (fingerprint, example));
        let mut reached = Vec::new();
        let mut approaching = Vec::new();
        let mut evidence = Vec::new();
        for finding in &findings {
            let reason = finding.render(top.map(|(fingerprint, _)| fingerprint));
            if finding.dimension == Dimension::Repeats {
                if let Some((_, example)) = top {
                    evidence.push(example.clone());
                }
            }
            match finding.level {
                Level::Reached => reached.push(reason),
                Level::Approaching => approaching.push(reason),
            }
        }
        Assessment {
            reached,
            approaching,
            evidence,
        }
    }

    /// The evidence a new policy stop records: the span whose arrival
    /// crossed the limit, and the repeated step when repeats are why.
    pub(crate) fn trip_evidence(&self, assessment: &Assessment) -> Vec<SpanRef> {
        let mut evidence: Vec<SpanRef> = self.last_span.iter().cloned().collect();
        for span in &assessment.evidence {
            if !evidence.contains(span) {
                evidence.push(span.clone());
            }
        }
        evidence
    }
}

/// What a run's limits make of it at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Assessment {
    /// Limits reached (or that the hypothetical next step would exceed).
    pub(crate) reached: Vec<String>,
    /// Limits at or past [`WARN_FRACTION`] but not reached.
    pub(crate) approaching: Vec<String>,
    /// Spans that illustrate a finding: the top repeat's example.
    pub(crate) evidence: Vec<SpanRef>,
}

/// Replays a run's stored spans, in step order, into a complete state.
///
/// `reset_at` is the run's last reset (server clock): spans that ENDED at
/// or before it are absorbed as cleared history. A policy stop is recorded
/// at the first span whose arrival would have tripped it live, timed by that
/// span's end — the replay has no admission clock to offer instead — or at
/// the reset itself when a reset left cumulative usage over a limit, which
/// live would re-trip at once. `resolve` names the run's policy from its
/// first span's service — the first span replayed, reset or not, exactly as
/// [`RunState::service`] has it.
pub(crate) fn replay(
    observations: &[Observation],
    reset_at: Option<u64>,
    resolve: &dyn Fn(&str) -> (Limits, Action),
) -> RunState {
    let mut state = RunState::new(true);
    let stop_limits = |service: &str| {
        let (limits, action) = resolve(service);
        (action == Action::Stop).then_some(limits)
    };
    // In step order no span starts before the first, so the run's service —
    // and with it the policy — is settled by the first span replayed.
    state.fold(observations, reset_at, &stop_limits, &|observation| {
        observation.end_ns
    });
    if let (Some(reset), None) = (reset_at, state.trip.as_ref()) {
        let (limits, action) = resolve(state.service.as_deref().unwrap_or(""));
        if action == Action::Stop {
            let assessment = state.assess(&limits, None);
            if !assessment.reached.is_empty() {
                state.trip = Some(Trip {
                    at_ns: reset,
                    evidence: state.trip_evidence(&assessment),
                    reasons: assessment.reached,
                });
            }
        }
    }
    state
}

/// Whether a run at `assessment` should warn, under `resolved`.
pub(crate) fn warns(assessment: &Assessment, resolved: &Resolved) -> bool {
    !assessment.approaching.is_empty()
        || (resolved.action == Action::Warn && !assessment.reached.is_empty())
}

// ------------------------------------------------------------ assessment

/// The numbers limits are compared against — copied out so an assessment
/// per replayed span costs no allocation until something is found.
#[derive(Clone, Copy, Debug, Default)]
struct Measures {
    cost_usd: f64,
    tokens: u64,
    steps: u64,
    llm_calls: u64,
    tool_calls: u64,
    duration_ns: u64,
    repeats: u64,
    error_streak: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dimension {
    Cost,
    Tokens,
    Steps,
    LlmCalls,
    ToolCalls,
    Duration,
    Repeats,
    ErrorStreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Approaching,
    Reached,
}

/// One limit that has something to say.
#[derive(Clone, Copy, Debug)]
struct Finding {
    dimension: Dimension,
    level: Level,
    limit: f64,
    /// Recorded usage.
    value: f64,
    /// What the hypothetical next step adds.
    delta: f64,
}

/// Compares `measures` (plus `next`) with every present limit.
///
/// Recorded usage at or past a limit has REACHED it — the budget is spent,
/// so the next step must not happen. A hypothetical next step reaches a
/// limit only by going PAST it: a run at 49 of 50 steps may take its
/// fiftieth, and after that the recorded count reaches the limit. That is
/// what keeps the pre-call answer and the post-acknowledgement one from
/// disagreeing by one step.
fn assess(measures: &Measures, limits: &Limits, next: Option<&NextStep>) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (dimension, limit, value, delta) in dimensions(measures, limits, next) {
        let Some(limit) = limit else { continue };
        let level = if value >= limit || (delta > 0.0 && value + delta > limit) {
            Level::Reached
        } else if value + delta >= limit * WARN_FRACTION {
            Level::Approaching
        } else {
            continue;
        };
        findings.push(Finding {
            dimension,
            level,
            limit,
            value,
            delta,
        });
    }
    findings
}

/// Whether recorded usage has reached any present limit: [`assess`]'s
/// `Reached` with no next step, without rendering a reason or allocating —
/// a fold asks it after every span, and a run near its limits would
/// otherwise format an `approaching` line per span only to drop it.
fn reaches(measures: &Measures, limits: &Limits) -> bool {
    dimensions(measures, limits, None)
        .iter()
        .any(|(_, limit, value, _)| limit.is_some_and(|limit| *value >= limit))
}

/// Every dimension with its limit, its recorded value and what `next` would
/// add — the one list [`assess`] and [`reaches`] both read, so a limit added
/// to one cannot be missing from the other.
fn dimensions(
    measures: &Measures,
    limits: &Limits,
    next: Option<&NextStep>,
) -> [(Dimension, Option<f64>, f64, f64); 8] {
    let step = next.map_or(0.0, |_| 1.0);
    let (llm, tool) = match next {
        Some(next) if next.tool.is_some() => (0.0, 1.0),
        Some(_) => (1.0, 0.0),
        None => (0.0, 0.0),
    };
    [
        (
            Dimension::Cost,
            limits.max_cost_usd,
            measures.cost_usd,
            next.and_then(|next| next.est_cost_usd).unwrap_or(0.0),
        ),
        (
            Dimension::Tokens,
            limits.max_tokens.map(|limit| limit as f64),
            measures.tokens as f64,
            next.and_then(|next| next.est_tokens).unwrap_or(0) as f64,
        ),
        (
            Dimension::Steps,
            limits.max_steps.map(|limit| limit as f64),
            measures.steps as f64,
            step,
        ),
        (
            Dimension::LlmCalls,
            limits.max_llm_calls.map(|limit| limit as f64),
            measures.llm_calls as f64,
            llm,
        ),
        (
            Dimension::ToolCalls,
            limits.max_tool_calls.map(|limit| limit as f64),
            measures.tool_calls as f64,
            tool,
        ),
        (
            Dimension::Duration,
            limits.max_duration_seconds.map(|limit| limit as f64 * 1e9),
            measures.duration_ns as f64,
            0.0,
        ),
        (
            Dimension::Repeats,
            limits.max_repeats.map(|limit| limit as f64),
            measures.repeats as f64,
            0.0,
        ),
        (
            Dimension::ErrorStreak,
            limits.max_error_streak.map(|limit| limit as f64),
            measures.error_streak as f64,
            0.0,
        ),
    ]
}

impl Finding {
    /// The reason line: the limit's name and value, then what the run did.
    fn render(&self, top_fingerprint: Option<&str>) -> String {
        let (name, limit) = match self.dimension {
            Dimension::Cost => ("max_cost_usd", usd(self.limit)),
            Dimension::Tokens => ("max_tokens", whole(self.limit)),
            Dimension::Steps => ("max_steps", whole(self.limit)),
            Dimension::LlmCalls => ("max_llm_calls", whole(self.limit)),
            Dimension::ToolCalls => ("max_tool_calls", whole(self.limit)),
            Dimension::Duration => ("max_duration_seconds", whole(self.limit / 1e9)),
            Dimension::Repeats => ("max_repeats", whole(self.limit)),
            Dimension::ErrorStreak => ("max_error_streak", whole(self.limit)),
        };
        let used = match self.dimension {
            Dimension::Cost => format!("${} spent", usd(self.value)),
            Dimension::Tokens => format!("{} tokens", whole(self.value)),
            Dimension::Steps => format!("{} steps", whole(self.value)),
            Dimension::LlmCalls => format!("{} LLM calls", whole(self.value)),
            Dimension::ToolCalls => format!("{} tool calls", whole(self.value)),
            Dimension::Duration => format!("the run spans {} s", whole(self.value / 1e9)),
            Dimension::Repeats => format!(
                "{} repeated {} times",
                truncate_chars(
                    top_fingerprint.unwrap_or("a step"),
                    REASON_FINGERPRINT_CHARS
                ),
                whole(self.value)
            ),
            Dimension::ErrorStreak => {
                format!("{} consecutive failed steps", whole(self.value))
            }
        };
        let with_next = match (self.dimension, self.delta > 0.0) {
            (_, false) => String::new(),
            (Dimension::Cost, true) => format!(" + ${} estimated", usd(self.delta)),
            (_, true) => format!(" + {} for the next step", whole(self.delta)),
        };
        match self.level {
            Level::Reached if self.value >= self.limit => {
                format!("{name} {limit} reached: {used}")
            }
            Level::Reached => {
                format!("{name} {limit} would be exceeded by the next step: {used}{with_next}")
            }
            Level::Approaching => {
                let percent = ((self.value + self.delta) / self.limit * 100.0).floor();
                format!("{name} {limit} at {percent:.0}%: {used}{with_next}")
            }
        }
    }
}

/// A dollar amount without trailing zeros: `10.42`, `0.000125`, `3`.
fn usd(value: f64) -> String {
    let fixed = format!("{value:.6}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A count or whole-second figure, floored.
fn whole(value: f64) -> String {
    format!("{:.0}", value.floor())
}

fn truncate_chars(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(id: &str, kind: StepKind, fingerprint: Option<&str>, error: bool) -> Observation {
        Observation {
            key: span_key("t", id),
            trace_id: "t".into(),
            span_id: id.into(),
            service: "svc".into(),
            kind,
            error,
            tokens: 10,
            cost_usd: 0.5,
            fingerprint: fingerprint.map(str::to_owned),
            start_ns: 1_000,
            end_ns: 2_000,
            revision: 0,
        }
    }

    fn step(id: &str, fingerprint: &str) -> Observation {
        obs(id, StepKind::Tool, Some(fingerprint), false)
    }

    #[test]
    fn usage_counts_steps_by_kind_and_everything_for_tokens_and_cost() {
        let mut run = RunState::new(true);
        run.absorb(&obs("a", StepKind::Llm, Some("llm:m#1"), false), false);
        run.absorb(&obs("b", StepKind::Tool, Some("tool:x#1"), true), false);
        run.absorb(&obs("c", StepKind::Agent, Some("agent:a"), false), false);
        run.absorb(&obs("d", StepKind::Other, None, true), false);
        let usage = run.usage();
        assert_eq!(
            (usage.steps, usage.llm_calls, usage.tool_calls),
            (3, 1, 1),
            "an `other` span is context, not a step"
        );
        assert_eq!((usage.tokens, usage.errors), (40, 2));
        assert!((usage.cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(
            usage.error_streak, 0,
            "the agent step succeeded after the failure"
        );
        assert!(usage.top_repeat.is_none(), "nothing occurred twice");
        assert_eq!(usage.duration_ns, 1_000);
    }

    #[test]
    fn a_reingested_span_replaces_its_contribution_and_a_retry_changes_nothing() {
        let mut run = RunState::new(true);
        let first = obs("a", StepKind::Tool, Some("tool:x#1"), true);
        assert!(run.absorb(&first, false));
        assert!(!run.absorb(&first, false), "an identical retry is a no-op");
        assert_eq!((run.steps, run.errors, run.error_streak), (1, 1, 1));

        let mut fixed = first.clone();
        fixed.error = false;
        fixed.tokens = 25;
        assert!(run.absorb(&fixed, false));
        let usage = run.usage();
        assert_eq!((usage.steps, usage.errors, usage.tokens), (1, 0, 25));
        assert_eq!(usage.error_streak, 0, "the re-sent version succeeded");
        assert_eq!(run.tracked(), 1);
    }

    #[test]
    fn repeats_track_the_top_fingerprint_and_its_latest_example() {
        let mut run = RunState::new(true);
        for id in ["a", "b", "c"] {
            run.absorb(&step(id, "tool:search#aaaa"), false);
        }
        run.absorb(&step("d", "tool:fetch#bbbb"), false);
        run.absorb(&step("e", "tool:fetch#bbbb"), false);
        let top = run.usage().top_repeat.expect("a repeat");
        assert_eq!(top.fingerprint, "tool:search#aaaa");
        assert_eq!(top.count, 3);
        assert_eq!(top.example.span_id, "c");

        // Re-sending two of the search steps with different input moves them
        // to another fingerprint: search falls to one and fetch leads.
        run.absorb(&step("a", "tool:other#cccc"), false);
        run.absorb(&step("b", "tool:other#dddd"), false);
        let top = run.usage().top_repeat.expect("a repeat");
        assert_eq!(
            (top.fingerprint.as_str(), top.count),
            ("tool:fetch#bbbb", 2)
        );
    }

    #[test]
    fn the_error_streak_counts_consecutive_failed_steps_only() {
        let mut run = RunState::new(true);
        run.absorb(&obs("a", StepKind::Tool, Some("t#1"), true), false);
        run.absorb(&obs("b", StepKind::Other, None, false), false);
        run.absorb(&obs("c", StepKind::Llm, Some("l#1"), true), false);
        assert_eq!(
            run.error_streak, 2,
            "a non-step span neither extends nor breaks it"
        );
        run.absorb(&obs("d", StepKind::Tool, Some("t#2"), false), false);
        assert_eq!(run.error_streak, 0);
    }

    #[test]
    fn a_reset_clears_the_episodic_detectors_but_not_the_budget() {
        let mut run = RunState::new(true);
        for id in ["a", "b", "c"] {
            run.absorb(&obs(id, StepKind::Tool, Some("tool:x#1"), true), false);
        }
        run.trip = Some(Trip {
            at_ns: 1,
            reasons: vec![],
            evidence: vec![],
        });
        run.reset();
        let usage = run.usage();
        assert!(run.trip.is_none());
        assert_eq!(
            (usage.steps, usage.errors),
            (3, 3),
            "cumulative usage stays"
        );
        assert_eq!(usage.error_streak, 0);
        assert!(usage.top_repeat.is_none());
        // A pre-reset span re-sent with new content keeps its side.
        let mut resent = obs("a", StepKind::Tool, Some("tool:x#1"), true);
        resent.tokens = 99;
        run.absorb(&resent, false);
        assert!(run.usage().top_repeat.is_none());
        assert_eq!(run.usage().error_streak, 0);
        run.absorb(&obs("d", StepKind::Tool, Some("tool:x#1"), true), false);
        run.absorb(&obs("e", StepKind::Tool, Some("tool:x#1"), true), false);
        assert_eq!(run.usage().top_repeat.map(|top| top.count), Some(2));
        assert_eq!(run.usage().error_streak, 2);
    }

    #[test]
    fn limits_warn_at_eighty_percent_and_reach_at_the_limit() {
        let mut run = RunState::new(true);
        let limits = Limits {
            max_steps: Some(5),
            max_cost_usd: Some(10.0),
            ..Limits::default()
        };
        for id in ["a", "b", "c"] {
            run.absorb(&step(id, &format!("tool:x#{id}")), false);
        }
        assert_eq!(run.assess(&limits, None), Assessment::default(), "3 of 5");
        run.absorb(&step("d", "tool:x#d"), false);
        let assessment = run.assess(&limits, None);
        assert_eq!(assessment.approaching, ["max_steps 5 at 80%: 4 steps"]);
        assert!(assessment.reached.is_empty());
        run.absorb(&step("e", "tool:x#e"), false);
        assert_eq!(
            run.assess(&limits, None).reached,
            ["max_steps 5 reached: 5 steps"]
        );
    }

    #[test]
    fn a_hypothetical_step_reaches_a_limit_only_by_passing_it() {
        let mut run = RunState::new(true);
        let limits = Limits {
            max_steps: Some(2),
            max_cost_usd: Some(1.0),
            ..Limits::default()
        };
        run.absorb(&step("a", "tool:x#a"), false);
        let next = NextStep {
            est_cost_usd: Some(0.4),
            ..NextStep::default()
        };
        // 1 step + 1 = 2, not past 2; $0.50 + $0.40 is 90% of $1.
        let assessment = run.assess(&limits, Some(&next));
        assert!(assessment.reached.is_empty(), "{assessment:?}");
        assert_eq!(
            assessment.approaching,
            [
                "max_cost_usd 1 at 90%: $0.5 spent + $0.4 estimated",
                "max_steps 2 at 100%: 1 steps + 1 for the next step",
            ]
        );
        let pricey = NextStep {
            est_cost_usd: Some(0.6),
            ..NextStep::default()
        };
        assert_eq!(
            run.assess(&limits, Some(&pricey)).reached,
            ["max_cost_usd 1 would be exceeded by the next step: $0.5 spent + $0.6 estimated"]
        );
    }

    #[test]
    fn replay_times_a_stop_at_the_crossing_span_and_honours_a_reset() {
        let observations: Vec<Observation> = (0..6)
            .map(|index| {
                let mut observation = step(&format!("s{index}"), "tool:loop#1");
                observation.end_ns = 1_000 + index;
                observation
            })
            .collect();
        let stop = |_: &str| {
            (
                Limits {
                    max_repeats: Some(3),
                    ..Limits::default()
                },
                Action::Stop,
            )
        };
        let run = replay(&observations, None, &stop);
        let trip = run.trip.expect("tripped");
        assert_eq!(trip.at_ns, 1_002, "the third occurrence crossed");
        assert_eq!(
            trip.reasons,
            ["max_repeats 3 reached: tool:loop#1 repeated 3 times"]
        );
        assert_eq!(trip.evidence[0].span_id, "s2");

        // A reset after the fourth span clears those four: the two after it
        // are not enough to trip again.
        let run = replay(&observations, Some(1_003), &stop);
        assert!(run.trip.is_none());
        assert_eq!(run.usage().top_repeat.map(|top| top.count), Some(2));
        assert_eq!(run.usage().steps, 6, "cumulative usage keeps every span");

        // A reset that leaves a cumulative limit spent re-trips at the reset.
        let budget = |_: &str| {
            (
                Limits {
                    max_steps: Some(4),
                    ..Limits::default()
                },
                Action::Stop,
            )
        };
        let run = replay(&observations[..5], Some(9_999), &budget);
        assert_eq!(run.trip.map(|trip| trip.at_ns), Some(9_999));

        // Under a warn action nothing is sticky.
        let warn = |_: &str| (Limits::default(), Action::Warn);
        assert!(replay(&observations, None, &warn).trip.is_none());
    }

    #[test]
    fn the_policy_follows_the_earliest_starting_span_live_and_in_replay() {
        let at = |id: &str, service: &str, start: u64, end: u64| {
            let mut observation = step(id, &format!("tool:x#{id}"));
            observation.service = service.into();
            observation.start_ns = start;
            observation.end_ns = end;
            observation
        };
        // Live: children arrive before the root that started first.
        let mut run = RunState::new(false);
        run.absorb(&at("child", "child-svc", 10, 20), false);
        assert_eq!(run.service.as_deref(), Some("child-svc"));
        run.absorb(&at("root", "root-svc", 5, 30), false);
        assert_eq!(run.service.as_deref(), Some("root-svc"));
        // A tie keeps the first arrival, so a retry cannot flip it.
        run.absorb(&at("twin", "twin-svc", 5, 30), false);
        assert_eq!(run.service.as_deref(), Some("root-svc"));

        // Replay resolves from the first span replayed, even when a reset
        // made a later span the first that counts.
        let resolved = std::cell::RefCell::new(Vec::new());
        let resolve = |service: &str| {
            resolved.borrow_mut().push(service.to_owned());
            (Limits::default(), Action::Stop)
        };
        let observations = [
            at("root", "root-svc", 5, 6),
            at("child", "child-svc", 10, 20),
        ];
        replay(&observations, Some(7), &resolve);
        // Asked during the fold and again for the reset's own check; both
        // times for the first span's service.
        let asked = resolved.borrow();
        assert!(
            !asked.is_empty() && asked.iter().all(|service| service == "root-svc"),
            "{asked:?}"
        );
    }

    #[test]
    fn a_span_contribution_stays_a_fixed_small_record() {
        // MAX_TRACKED_SPANS is documented in these terms; a field added to
        // Mark has to be a decision about that bound, not a side effect. The
        // write revision took it from 32 to 40: without it a late observer
        // can put back a version the store replaced.
        assert_eq!(std::mem::size_of::<Mark>(), 40);
    }

    #[test]
    fn an_older_version_observed_last_changes_nothing() {
        let at = |revision: u64, error: bool, tokens: u64| {
            let mut observation = obs("a", StepKind::Tool, Some("tool:x#1"), error);
            observation.revision = revision;
            observation.tokens = tokens;
            observation
        };
        let mut run = RunState::new(true);
        assert!(run.absorb(&at(7, true, 50), false), "the newer version");
        assert!(!run.absorb(&at(5, false, 10), false), "the older, late");
        let usage = run.usage();
        assert_eq!((usage.tokens, usage.errors, usage.error_streak), (50, 1, 1));
        // An unchanged retry still advances the revision, so the version
        // written between it and the first cannot land after it.
        assert!(!run.absorb(&at(9, true, 50), false));
        assert!(!run.absorb(&at(8, false, 10), false));
        assert_eq!(run.usage().tokens, 50);

        // Below a rebuild's floor, nothing lands: the read already holds
        // that version or a newer one — of this span or of one never seen.
        run.floor = 20;
        assert!(!run.absorb(&at(15, false, 99), false));
        let mut unseen = obs("b", StepKind::Tool, Some("tool:x#2"), false);
        unseen.revision = 12;
        assert!(!run.absorb(&unseen, false));
        assert_eq!((run.usage().tokens, run.tracked()), (50, 1));
        assert!(run.absorb(&at(21, false, 10), false));
        assert_eq!(run.usage().tokens, 10);
    }

    #[test]
    fn a_reingested_span_that_only_moved_its_clocks_moves_the_runs() {
        let mut run = RunState::new(true);
        let first = obs("a", StepKind::Tool, Some("tool:x#1"), false);
        assert!(run.absorb(&first, false));
        assert_eq!(run.usage().duration_ns, 1_000);
        let mut later = first.clone();
        later.end_ns = 9_000;
        assert!(run.absorb(&later, false), "the run's end moved");
        assert_eq!((run.usage().duration_ns, run.usage().steps), (8_000, 1));
        // Narrowing back is not undone — the bounds only widen — and is no
        // change at all.
        assert!(!run.absorb(&first, false));
        assert_eq!(run.usage().duration_ns, 8_000);
    }

    #[test]
    fn a_fold_latches_a_stop_crossed_mid_batch_and_cleared_by_its_end() {
        let streak = |_: &str| {
            Some(Limits {
                max_error_streak: Some(2),
                ..Limits::default()
            })
        };
        let batch = [
            obs("a", StepKind::Tool, Some("t#a"), true),
            obs("b", StepKind::Tool, Some("t#b"), true),
            obs("c", StepKind::Tool, Some("t#c"), false),
        ];
        let mut run = RunState::new(true);
        assert!(run.fold(&batch, None, &streak, &|_| 42));
        let trip = run.trip.clone().expect("latched");
        assert_eq!((trip.at_ns, trip.evidence[0].span_id.as_str()), (42, "b"));
        assert_eq!(run.usage().error_streak, 0, "the success ended it");
        // Nothing latches twice, and nothing latches where nothing can.
        assert!(!run.fold(&batch, None, &streak, &|_| 43));
        let mut open = RunState::new(true);
        assert!(!open.fold(&batch, None, &|_| None, &|_| 42));
        assert!(open.trip.is_none());
    }

    #[test]
    fn a_fold_judges_each_span_once_at_its_stored_place_in_step_order() {
        let streak = |_: &str| {
            Some(Limits {
                max_error_streak: Some(2),
                ..Limits::default()
            })
        };
        let at = |id: &str, start: u64, error: bool, revision: u64| {
            let mut observation = obs(id, StepKind::Tool, Some(&format!("t#{id}")), error);
            observation.start_ns = start;
            observation.end_ns = start + 5;
            observation.revision = revision;
            observation
        };
        // A's second version, the one stored, moved it after the success:
        // in step order no two failures are adjacent.
        let batch = [
            at("a", 10, true, 1),
            at("b", 20, true, 2),
            at("c", 30, false, 3),
            at("a", 40, true, 4),
        ];
        let mut run = RunState::new(true);
        assert!(!run.fold(&batch, None, &streak, &|_| 42));
        let usage = run.usage();
        assert_eq!((usage.errors, usage.error_streak, run.tracked()), (2, 1, 3));
        // What arrives during a rebuild is gathered over several batches, so
        // it is out of step order and the older version of a span can come
        // last. Still one span, judged where the stored version puts it.
        let gathered = [
            at("c", 30, false, 3),
            at("a", 40, true, 4),
            at("b", 20, true, 2),
            at("a", 10, true, 1),
        ];
        let mut run = RunState::new(true);
        assert!(!run.fold(&gathered, None, &streak, &|_| 42));
        let usage = run.usage();
        assert_eq!((usage.errors, usage.error_streak, run.tracked()), (2, 1, 3));
    }

    #[test]
    fn a_fold_takes_the_policy_of_its_earliest_span_before_judging_any() {
        let at = |id: &str, service: &str, start: u64| {
            let mut observation = step(id, &format!("tool:x#{id}"));
            observation.service = service.into();
            observation.start_ns = start;
            observation.end_ns = start + 5;
            observation
        };
        let asked = std::cell::RefCell::new(Vec::new());
        let limits = |service: &str| {
            asked.borrow_mut().push(service.to_owned());
            Some(Limits {
                max_steps: Some(if service == "root-svc" { 100 } else { 1 }),
                ..Limits::default()
            })
        };
        // Children first, then the root that started before them: the
        // child service's one-step limit is never the run's.
        let mut run = RunState::new(true);
        let batch = [at("child", "child-svc", 20), at("root", "root-svc", 10)];
        assert!(!run.fold(&batch, None, &limits, &|_| 42));
        assert_eq!(run.service.as_deref(), Some("root-svc"));
        // Likewise when the run already has spans, all of them later.
        let mut run = RunState::new(true);
        run.absorb(&at("early", "late-svc", 50), false);
        let batch = [at("more", "late-svc", 60), at("root", "root-svc", 10)];
        assert!(!run.fold(&batch, None, &limits, &|_| 42));
        assert_eq!(run.service.as_deref(), Some("root-svc"));
        let asked = asked.borrow();
        assert!(
            asked.iter().all(|service| service == "root-svc"),
            "{asked:?}"
        );
    }

    #[test]
    fn money_and_counts_render_without_noise() {
        assert_eq!(usd(10.42), "10.42");
        assert_eq!(usd(0.1 + 0.2), "0.3");
        assert_eq!(usd(3.0), "3");
        assert_eq!(usd(0.0), "0");
        assert_eq!(whole(12.9), "12");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }
}
