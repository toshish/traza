//! Run guard: fuses that warn or stop an agent run against its limits.
//!
//! A run (see [`crate::semconv::run_id`]) is held to one [`FusePolicy`] —
//! limits on cost, tokens, steps, LLM and tool calls, duration, repeated
//! steps and consecutive failures — and has a [`Verdict`]: `continue`, `warn`
//! at 80% of any limit, and at 100% whatever the policy's action says, where
//! a `stop` is STICKY until the run is reset. An operator can also stop a run
//! outright (the kill switch) and reset one, optionally granting it higher
//! limits.
//!
//! # The ack is the brake, and it brakes the NEXT step
//!
//! Spans arrive when operations END. By the time a step's span is ingested,
//! the step has already happened, so a verdict raised on the acknowledgement
//! of a batch ([`crate::Admission::signals`], which `traza-server` returns as
//! a `fuses` field and a `Traza-Fuse` header) can only stop what comes after
//! it. The pre-call gate is [`Store::fuse_check`]: asked before a step, with
//! the step's estimated cost, it answers whether the step would cross a stop
//! limit. Both read the same state, so they agree on where the limit is.
//!
//! # State, and what survives a restart
//!
//! Usage is a bounded in-memory cache over the store, folded from every
//! acknowledged batch by the ingest observer — O(batch), with no lock held
//! while a span's facts are extracted. Every run is rebuilt from the store
//! the first time a verdict is asked for it, under the query deadline: the
//! process cannot know whether a run had spans before it began following it
//! (the run may predate a restart, or have been evicted), so it never assumes
//! not. What spans cannot reconstruct — policies, manual trips, resets and
//! grants — is durable in `fuses.jsonl` (see the `log` module).
//!
//! Live runs are bounded: at most [`FuseConfig::max_runs`], least recently
//! active evicted first, idle ones dropped after six hours, and at most
//! [`MAX_TRACKED_SPANS`] span contributions held across all of them — with
//! no exceptions, so nothing can keep the bound from holding. A run evicted
//! while STOPPED is not forgotten: it is parked, keeping its stop (reasons,
//! time, evidence) and its usage as last seen but none of its per-span
//! state, so a policy stop stays sticky through eviction exactly as a kill
//! switch does. Parked runs are bounded by the same `max_runs`, oldest
//! dropped first; only a policy stop dropped that way is lost, and a rebuild
//! then recomputes it under the policy in force. An evicted run that was not
//! stopped loses its live usage, which a rebuild restores, and the latch
//! that a warn was already announced.
//!
//! # Erasure
//!
//! Guard state is derived from spans, so an erasure must reach it, and must
//! not be undone by work already in flight when it ran. Every erasure pass
//! bumps an epoch: a rebuild that read the store before the pass discards
//! what it read and reads again. And the ingest observer skips spans a
//! pending erasure covers: a batch acknowledged just before an erasure began
//! reaches the observer after its fsync, which can be after the erasure
//! swept the guard, and the mask is up from the erasure's start to its
//! settle.

mod events;
pub mod http;
mod log;
mod policy;
mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use events::{EventKind, EventPage, FuseEvent, MAX_WAIT_MS};
pub use policy::{
    Action, FuseConfig, FusePolicy, Limits, PolicyMatch, PolicySpec, DEFAULT_MAX_REPEATS,
    DEFAULT_MAX_RUNS, DEFAULT_POLICY_NAME, GLOBAL_POLICY_NAME, MAX_POLICIES_PER_TENANT,
    MAX_SERVICE_BYTES,
};
pub use state::{NextStep, SpanRef, TopRepeat, TrippedBy, Usage};

use log::{FuseLog, Record};
use state::{Observation, RunState, Trip};

use crate::{erasure, semconv, Deadline, Error, Result, Span, Store};

/// Ceiling on span contributions held across every live run — each a 32-byte
/// record under an 8-byte key, before hash-table overhead. Past it, whole
/// runs are evicted least-recently-active first; they rebuild from the store
/// when next asked about.
pub const MAX_TRACKED_SPANS: usize = 1_000_000;

/// Most spans a rebuild reads for one run. A larger run is rebuilt from its
/// earliest spans, and undercounts.
pub const REBUILD_MAX_SPANS: usize = 100_000;

/// Longest run id the guard accepts or follows, in bytes. Every live run
/// holds its id, and a question about a run can come from a read-only
/// credential, so an unbounded id would be unbounded memory per request. A
/// span whose run id is longer is not followed (nothing could ask about it).
pub const MAX_RUN_ID_BYTES: usize = 1024;

/// Most runs one tenant may hold durable controls for — a kill switch, a
/// reset, a grant. Past it, the controls of the run reset longest ago are
/// dropped to make room (its grant and reset stop applying, which can only
/// make it stricter), and when every one of them is an active kill switch a
/// new trip or reset is refused: a kill switch is never dropped.
pub const MAX_CONTROLLED_RUNS_PER_TENANT: usize = 10_000;

/// A live run idle this long (server clock) is evicted.
const IDLE_NS: u64 = 6 * 60 * 60 * 1_000_000_000;

/// Longest manual-trip reason accepted, in bytes.
const MAX_REASON_BYTES: usize = 1024;

/// Times a question about a run retries a rebuild an erasure overlapped
/// before giving up with a retryable conflict.
const REBUILD_ATTEMPTS: usize = 3;

/// Spans a rebuild extracts between deadline checks.
const DEADLINE_STRIDE: usize = 1024;

/// Live runs re-judged per acquisition of the state lock when a policy
/// edit re-settles a tenant, so a tenant with many runs cannot hold every
/// ingest observer off for the whole pass.
const SETTLE_CHUNK: usize = 256;

/// What a run should do next.
///
/// Ordered by severity — `Continue < Warn < Stop` — so the most severe of
/// several verdicts is their `max()`.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing to report; the run may proceed.
    #[default]
    Continue,
    /// The run is approaching a limit (or reached one under a `warn`
    /// policy); it may proceed.
    Warn,
    /// The run crossed a limit, or was stopped by hand, and should stop.
    Stop,
}

impl Verdict {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Warn => "warn",
            Self::Stop => "stop",
        }
    }

    /// The listing index slot this class is filed under.
    fn slot(self) -> usize {
        match self {
            Self::Continue => 0,
            Self::Warn => 1,
            Self::Stop => 2,
        }
    }
}

/// A verdict about one run, raised while admitting a batch that touched it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The run id ([`crate::semconv::run_id`]).
    pub run: String,
    /// The verdict.
    pub verdict: Verdict,
    /// Why, one human-readable reason per line of evidence.
    pub reasons: Vec<String>,
}

/// The `Traza-Fuse` header value for a batch's signals: the most severe
/// verdict and its run, as `stop; run=<id>` or `warn; run=<id>`, or `None`
/// when nothing was raised.
///
/// The run id comes from telemetry, so every byte outside a conservative
/// visible-ASCII set — and `%`, `;`, `,`, `"` and space — is percent-encoded:
/// a session id holding a CR LF must not be able to write a header of its
/// own.
pub fn fuse_header(signals: &[Signal]) -> Option<String> {
    let worst = signals
        .iter()
        .filter(|signal| signal.verdict != Verdict::Continue)
        .max_by_key(|signal| signal.verdict)?;
    let mut run = String::with_capacity(worst.run.len());
    for byte in worst.run.bytes() {
        match byte {
            b'!'..=b'~' if !matches!(byte, b'%' | b';' | b',' | b'"') => run.push(byte as char),
            other => run.push_str(&format!("%{other:02X}")),
        }
    }
    Some(format!("{}; run={run}", worst.verdict.as_str()))
}

/// Where a verdict document's usage came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Answered from memory, without reading the store.
    Live,
    /// Answering required rebuilding the run from its stored spans.
    Rebuilt,
}

/// Everything the guard says about one run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VerdictDoc {
    /// The run id.
    pub run: String,
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The verdict.
    pub verdict: Verdict,
    /// Why: for a stop, the reasons recorded when it tripped; otherwise the
    /// limits reached or approached now. Empty for `continue`.
    pub reasons: Vec<String>,
    /// The name of the policy the run is held to.
    pub policy: String,
    /// What the run has used.
    pub usage: Usage,
    /// The limits in force: the policy's, with the run's grant over them.
    pub limits: Limits,
    /// Limits raised for this run by resets, or `None`.
    pub grant: Option<Limits>,
    /// When the stop tripped: server clock for a live or manual stop, the
    /// crossing span's end for one a rebuild recomputed.
    pub tripped_at_ns: Option<u64>,
    /// Who stopped the run, when it is stopped.
    pub tripped_by: Option<TrippedBy>,
    /// Spans behind the verdict: the one whose arrival tripped a stop, and
    /// the repeated step when repeats are a reason.
    pub evidence: Vec<SpanRef>,
    /// Whether answering read the store.
    pub source: Source,
}

/// Which runs a listing returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunListFilter {
    /// Every run the guard holds: live ones, and stopped ones parked out of
    /// the live set.
    #[default]
    Active,
    /// Runs whose verdict is `warn`.
    Warn,
    /// Runs whose verdict is `stop`.
    Stop,
}

impl RunListFilter {
    /// Parses `active`, `warn` or `stop`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "warn" => Some(Self::Warn),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    /// The verdict classes this filter lists.
    fn classes(self) -> &'static [Verdict] {
        match self {
            Self::Active => &[Verdict::Stop, Verdict::Warn, Verdict::Continue],
            Self::Warn => &[Verdict::Warn],
            Self::Stop => &[Verdict::Stop],
        }
    }
}

/// A run within its tenant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct RunKey {
    tenant: String,
    run: String,
}

impl RunKey {
    fn new(tenant: &str, run: &str) -> Self {
        Self {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
        }
    }
}

/// The durable controls on one run, replayed from the log.
#[derive(Clone, Debug, Default)]
struct Controls {
    /// An active kill switch: when, and why.
    manual: Option<(u64, String)>,
    /// The last reset, server clock.
    last_reset_ns: Option<u64>,
    /// Every grant so far, merged (later grants win per limit).
    grant: Option<Limits>,
}

/// A stopped run evicted from the live set: its stop and its last-seen
/// usage, without the per-span state that made it expensive to keep.
#[derive(Clone, Debug)]
struct Parked {
    /// Recency position, carried over from when the run was live.
    tick: u64,
    /// The sticky policy stop; `None` for a run stopped by hand only (the
    /// kill switch itself lives in its controls).
    trip: Option<Trip>,
    /// The run's first span's service, which picks its policy.
    service: Option<String>,
    /// Usage as last seen live, for listings.
    usage: Usage,
}

impl Parked {
    /// Spans the parked state names.
    fn span_refs(&self) -> impl Iterator<Item = &SpanRef> {
        self.trip
            .iter()
            .flat_map(|trip| trip.evidence.iter())
            .chain(self.usage.top_repeat.iter().map(|top| &top.example))
    }
}

/// Run keys by tick per verdict class — across every tenant, and per tenant
/// — covering live and parked runs alike. A listing walks only the classes
/// and the tenant it asks for, newest first, and stops at its limit, so its
/// cost under the state lock follows what it returns, not how many runs
/// exist.
#[derive(Default)]
struct Listing {
    all: [BTreeMap<u64, Arc<RunKey>>; 3],
    by_tenant: HashMap<String, [BTreeMap<u64, Arc<RunKey>>; 3]>,
}

impl Listing {
    fn insert(&mut self, key: &Arc<RunKey>, tick: u64, class: Verdict) {
        self.all[class.slot()].insert(tick, Arc::clone(key));
        // Looked up before inserting, so the common case — a tenant already
        // indexed — allocates no copy of its name.
        match self.by_tenant.get_mut(&key.tenant) {
            Some(maps) => {
                maps[class.slot()].insert(tick, Arc::clone(key));
            }
            None => {
                let mut maps: [BTreeMap<u64, Arc<RunKey>>; 3] = Default::default();
                maps[class.slot()].insert(tick, Arc::clone(key));
                self.by_tenant.insert(key.tenant.clone(), maps);
            }
        }
    }

    /// Moves a run from `previous` to `tick` within its class: the touch
    /// every observed batch makes, so it allocates nothing.
    fn retick(&mut self, key: &RunKey, previous: u64, tick: u64, class: Verdict) {
        if let Some(shared) = self.all[class.slot()].remove(&previous) {
            self.all[class.slot()].insert(tick, shared);
        }
        if let Some(maps) = self.by_tenant.get_mut(&key.tenant) {
            if let Some(shared) = maps[class.slot()].remove(&previous) {
                maps[class.slot()].insert(tick, shared);
            }
        }
    }

    fn remove(&mut self, key: &RunKey, tick: u64, class: Verdict) {
        self.all[class.slot()].remove(&tick);
        if let Some(maps) = self.by_tenant.get_mut(&key.tenant) {
            maps[class.slot()].remove(&tick);
            if maps.iter().all(BTreeMap::is_empty) {
                self.by_tenant.remove(&key.tenant);
            }
        }
    }

    /// At most `limit` keys in `classes`, newest first, from `tenant` or
    /// from every tenant.
    fn newest(&self, tenant: Option<&str>, classes: &[Verdict], limit: usize) -> Vec<Arc<RunKey>> {
        let maps = match tenant {
            Some(tenant) => match self.by_tenant.get(tenant) {
                Some(maps) => maps,
                None => return Vec::new(),
            },
            None => &self.all,
        };
        let mut cursors: Vec<_> = classes
            .iter()
            .map(|class| maps[class.slot()].iter().rev().peekable())
            .collect();
        let mut keys = Vec::new();
        while keys.len() < limit {
            let newest = cursors
                .iter_mut()
                .enumerate()
                .filter_map(|(index, cursor)| cursor.peek().map(|(tick, _)| (**tick, index)))
                .max();
            let Some((_, index)) = newest else { break };
            if let Some((_, key)) = cursors[index].next() {
                keys.push(Arc::clone(key));
            }
        }
        keys
    }

    /// Every key of `tenant`, any class.
    fn tenant_keys(&self, tenant: &str) -> Vec<Arc<RunKey>> {
        self.by_tenant
            .get(tenant)
            .map(|maps| maps.iter().flat_map(BTreeMap::values).cloned().collect())
            .unwrap_or_default()
    }
}

/// The guard's mutable state, behind one leaf lock that is never held while
/// the store is read or the log is written.
#[derive(Default)]
struct State {
    /// Policies, by tenant.
    policies: HashMap<String, Vec<FusePolicy>>,
    /// Durable controls, by tenant and then run.
    controls: HashMap<String, HashMap<String, Controls>>,
    runs: HashMap<Arc<RunKey>, RunState>,
    /// Live runs by recency: tick → run. The oldest tick is evicted first.
    order: BTreeMap<u64, Arc<RunKey>>,
    /// Stopped runs evicted from the live set.
    parked: HashMap<Arc<RunKey>, Parked>,
    /// Parked runs by recency; the oldest is dropped first.
    parked_order: BTreeMap<u64, Arc<RunKey>>,
    /// Live and parked runs by class, for listings.
    listing: Listing,
    next_tick: u64,
    /// Marks held across every live run.
    tracked: usize,
    /// Runs being rebuilt: the observations that arrived while their stored
    /// spans were being read, and how many rebuilds are in flight.
    rebuilding: HashMap<RunKey, (usize, Vec<Observation>)>,
    /// Bumped by every erasure pass, so a rebuild can tell that what it read
    /// may include what an erasure has since removed.
    erasure_epoch: u64,
}

/// The run guard a store owns when fuses are enabled.
pub(crate) struct Guard {
    config: FuseConfig,
    /// Serializes every durable mutation — log append, then state update —
    /// so the log's order is the order the state saw. Taken BEFORE the state
    /// lock, never after; ingest takes only the state lock and so never
    /// waits on an fsync.
    log: Mutex<FuseLog>,
    state: Mutex<State>,
    events: events::EventFeed,
    trips: AtomicU64,
    warnings: AtomicU64,
    /// Runs once, at the end of the next erasure sweep: lets a test land an
    /// ingest observer exactly where a late one would — after the guard was
    /// swept, while the erasure is still pending.
    #[cfg(test)]
    after_forget: Mutex<Option<ForgetHook>>,
}

/// See [`Guard::after_forget`].
#[cfg(test)]
type ForgetHook = Box<dyn FnOnce(&Store) + Send>;

impl Guard {
    /// Opens the guard over `directory`, replaying `fuses.jsonl`; `None` when
    /// fuses are disabled, in which case the log is not even read — so a
    /// damaged log cannot keep a `--no-fuses` server from starting.
    pub(crate) fn open(directory: &Path, config: &FuseConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let (mut log, records) = FuseLog::open(directory)?;
        let mut state = State::default();
        for record in &records {
            state.apply(record);
        }
        // Runs whose controls were dropped to make room are dropped from the
        // log only at its next rewrite, so replay can bring them back.
        state.trim_controls();
        // A kill switch outlives a restart, and so does its run's place in
        // the stopped list: every manually stopped run is parked — oldest
        // trip first, so the listing's recency order is the trips' — to be
        // rebuilt when asked about.
        let mut stopped: Vec<(u64, RunKey)> = state
            .controls
            .iter()
            .flat_map(|(tenant, runs)| {
                runs.iter().filter_map(move |(run, controls)| {
                    let (at_ns, _) = controls.manual.as_ref()?;
                    Some((*at_ns, RunKey::new(tenant, run)))
                })
            })
            .collect();
        stopped.sort();
        for (_, key) in stopped {
            let tick = state.bump();
            state.park(
                Arc::new(key),
                Parked {
                    tick,
                    trip: None,
                    service: None,
                    usage: Usage::default(),
                },
            );
        }
        state.trim_parked(config.max_runs);
        if log.compaction_due() {
            log.compact(&state.records())?;
        }
        Ok(Some(Self {
            config: config.clone(),
            log: Mutex::new(log),
            state: Mutex::new(state),
            events: events::EventFeed::new(),
            trips: AtomicU64::new(0),
            warnings: AtomicU64::new(0),
            #[cfg(test)]
            after_forget: Mutex::new(None),
        }))
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| Error::LockPoisoned("fuses"))
    }

    fn lock_log(&self) -> Result<MutexGuard<'_, FuseLog>> {
        self.log.lock().map_err(|_| Error::LockPoisoned("fuse log"))
    }

    /// Folds an acknowledged batch in and returns a signal for every run in
    /// it whose verdict is not `continue`.
    fn observe(&self, store: &Store, spans: &[Arc<Span>]) -> Vec<Signal> {
        // A span a pending erasure covers is skipped: this batch may have
        // been acknowledged just before the erasure began, with this
        // observer running after the erasure already swept the guard.
        let mask = store.erasure_mask();
        // Extraction — facts, step kinds, fingerprint digests — happens with
        // no lock held, grouped by run in first-appearance order.
        let mut index: HashMap<RunKey, usize> = HashMap::new();
        let mut groups: Vec<(RunKey, Vec<Observation>)> = Vec::new();
        for span in spans {
            if mask
                .as_deref()
                .is_some_and(|mask| mask.covers_for_drop(span))
            {
                continue;
            }
            let run = semconv::run_id(&span.attributes, &span.trace_id);
            if run.len() > MAX_RUN_ID_BYTES {
                continue;
            }
            let key = RunKey {
                tenant: span.tenant.clone(),
                run,
            };
            let observation = Observation::of(store, span);
            let slot = *index.entry(key.clone()).or_insert_with(|| {
                groups.push((key, Vec::new()));
                groups.len() - 1
            });
            groups[slot].1.push(observation);
        }
        let now = unix_now_ns();
        // The observer cannot fail an acknowledged ingest; a poisoned state
        // costs this batch its verdicts, not its durability.
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let mut signals = Vec::new();
        for (key, observations) in groups {
            if let Some((_, pending)) = state.rebuilding.get_mut(&key) {
                pending.extend(observations.iter().cloned());
            } else if !state.runs.contains_key(&key)
                && !state.parked.contains_key(&key)
                && !observations.iter().any(Observation::is_step)
            {
                // A run starts being followed at its first step. A trace of
                // HTTP and database spans is a "run" by identity too, but it
                // has nothing a fuse limits that a step would not bring, so
                // ordinary tracing traffic costs the observer this read and
                // no state. A later step starts following it from there,
                // and the first verdict asked for it rebuilds it whole from
                // the store, these spans included. A parked run is a stopped
                // one, and any span of it hears so.
                continue;
            }
            state.entry(&key, now);
            let added = {
                let run = state.runs.get_mut(&key).expect("entry just ensured");
                let before = run.tracked();
                for observation in &observations {
                    run.absorb(observation, false);
                }
                run.tracked() - before
            };
            state.tracked += added;
            let judged = self.settle(&mut state, &key, now);
            if judged.verdict != Verdict::Continue {
                signals.push(Signal {
                    tenant: key.tenant.clone(),
                    run: key.run.clone(),
                    verdict: judged.verdict,
                    reasons: judged.reasons,
                });
            }
        }
        state.evict(now, self.config.max_runs, MAX_TRACKED_SPANS, None);
        signals
    }

    /// Re-judges one live run against its current policy and records the
    /// transitions that produces: a new policy stop (sticky, counted,
    /// announced) and a new warn (counted, announced once).
    fn settle(&self, state: &mut State, key: &RunKey, now: u64) -> Judgement {
        let judged = state.judge(&self.config, key, None, now);
        let Some(run) = state.runs.get_mut(key) else {
            return judged;
        };
        if judged.new_trip.is_some() && run.trip.is_none() {
            run.trip = judged.new_trip.clone();
            self.trips.fetch_add(1, Ordering::Relaxed);
            self.announce(key, EventKind::Stop, &judged, now);
        } else if judged.verdict == Verdict::Warn {
            // Announced when the level RISES: entering warn, and again when
            // a warn policy's limit is reached — the moment a stop policy
            // would have stopped the run, which a dispatcher wants to hear
            // about separately from "approaching".
            if judged.warn_level > run.warn_level {
                self.warnings.fetch_add(1, Ordering::Relaxed);
                self.announce(key, EventKind::Warn, &judged, now);
            }
            run.warn_level = judged.warn_level;
        } else if judged.verdict == Verdict::Continue {
            run.warn_level = 0;
        }
        state.reclass(key);
        judged
    }

    fn announce(&self, key: &RunKey, kind: EventKind, judged: &Judgement, now: u64) {
        self.events.push(FuseEvent {
            cursor: String::new(),
            at_ns: now,
            tenant: key.tenant.clone(),
            run: key.run.clone(),
            kind,
            reasons: judged.reasons.clone(),
            policy: judged.policy.clone(),
        });
    }

    /// Re-settles every live run of `tenant` after its policies changed, so
    /// a tightened limit trips now rather than at the run's next batch. A
    /// chunk at a time: the tenant's runs are found through the listing
    /// index, and the lock is released between chunks so ingest observers
    /// interleave.
    fn settle_tenant(&self, tenant: &str, now: u64) -> Result<()> {
        let keys = self.lock_state()?.listing.tenant_keys(tenant);
        for chunk in keys.chunks(SETTLE_CHUNK) {
            let mut state = self.lock_state()?;
            for key in chunk {
                if state.runs.contains_key(&**key) {
                    self.settle(&mut state, key, now);
                }
            }
        }
        Ok(())
    }

    /// After a durable mutation, with the log lock held: compacts the log
    /// when enough of it is superseded. The records are taken under the
    /// state lock and written after it is released, so no ingest observer
    /// waits on the rewrite's fsync; the log lock keeps every other mutation
    /// out until it is done.
    fn compact_if_due(&self, log: &mut FuseLog, state: MutexGuard<'_, State>) -> Result<()> {
        if !log.compaction_due() {
            return Ok(());
        }
        let records = state.records();
        drop(state);
        log.compact(&records)?;
        Ok(())
    }
}

/// Everything durable a rebuild's replay depends on besides the spans.
#[derive(Clone, Debug, PartialEq)]
struct ReplayInputs {
    reset_at: Option<u64>,
    grant: Option<Limits>,
    policies: Vec<FusePolicy>,
}

impl ReplayInputs {
    /// Replays `observations` (in step order) into a complete run state.
    fn replay(&self, config: &FuseConfig, tenant: &str, observations: &[Observation]) -> RunState {
        let resolve = |service: &str| {
            let resolved = policy::resolve(&self.policies, config, tenant, service);
            let limits = match &self.grant {
                Some(grant) => resolved.limits.overlaid(grant),
                None => resolved.limits,
            };
            (limits, resolved.action)
        };
        state::replay(observations, self.reset_at, &resolve)
    }
}

/// A run judged at one moment.
#[derive(Clone, Debug, Default)]
struct Judgement {
    verdict: Verdict,
    reasons: Vec<String>,
    policy: String,
    limits: Limits,
    grant: Option<Limits>,
    tripped_at_ns: Option<u64>,
    tripped_by: Option<TrippedBy>,
    evidence: Vec<SpanRef>,
    /// For a warn: 1 when limits are only approached, 2 when one is reached
    /// under a `warn` policy.
    warn_level: u8,
    /// A policy stop this judgement found that the run does not carry yet.
    new_trip: Option<Trip>,
}

/// What one rebuild read, before it is installed.
struct RebuildRead {
    /// The erasure epoch when the rebuild registered.
    epoch: u64,
    /// The extracted spans, the replay inputs, and the replayed state.
    replayed: Result<(Vec<Observation>, ReplayInputs, RunState)>,
}

impl State {
    fn bump(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
    }

    /// The tenant's policies; empty when it has none.
    fn tenant_policies(&self, tenant: &str) -> &[FusePolicy] {
        self.policies.get(tenant).map_or(&[], Vec::as_slice)
    }

    fn controls_of(&self, key: &RunKey) -> Option<&Controls> {
        self.controls.get(&key.tenant)?.get(&key.run)
    }

    fn controls_mut(&mut self, key: &RunKey) -> &mut Controls {
        self.controls
            .entry(key.tenant.clone())
            .or_default()
            .entry(key.run.clone())
            .or_default()
    }

    /// Applies one durable record, in log order.
    fn apply(&mut self, record: &Record) {
        match record {
            Record::Policy { policy } => {
                let policies = self.policies.entry(policy.tenant.clone()).or_default();
                policies
                    .retain(|existing| existing.id != policy.id && existing.name != policy.name);
                policies.push(policy.clone());
            }
            Record::DeletePolicy { id, .. } => {
                for policies in self.policies.values_mut() {
                    policies.retain(|policy| &policy.id != id);
                }
                self.policies.retain(|_, policies| !policies.is_empty());
            }
            Record::Trip {
                tenant,
                run,
                reason,
                at_ns,
            } => {
                self.controls_mut(&RunKey::new(tenant, run)).manual =
                    Some((*at_ns, reason.clone()));
            }
            Record::Reset {
                tenant,
                run,
                grant,
                at_ns,
            } => {
                let controls = self.controls_mut(&RunKey::new(tenant, run));
                controls.manual = None;
                controls.last_reset_ns = Some(*at_ns);
                if let Some(grant) = grant {
                    controls.grant = Some(match controls.grant {
                        Some(earlier) => earlier.overlaid(grant),
                        None => *grant,
                    });
                }
            }
        }
    }

    /// Makes room for `key` among its tenant's controlled runs before a
    /// trip or reset adds it: drops the controls of the run reset longest
    /// ago when the tenant is at [`MAX_CONTROLLED_RUNS_PER_TENANT`], and
    /// refuses when every one of them is an active kill switch. Returns the
    /// run whose controls were dropped, which its caller re-judges (a
    /// dropped grant can make a live run stricter).
    fn room_for_control(&mut self, key: &RunKey) -> Result<Option<RunKey>> {
        let Some(runs) = self.controls.get_mut(&key.tenant) else {
            return Ok(None);
        };
        if runs.contains_key(&key.run) || runs.len() < MAX_CONTROLLED_RUNS_PER_TENANT {
            return Ok(None);
        }
        let oldest = runs
            .iter()
            .filter(|(_, controls)| controls.manual.is_none())
            .min_by(|left, right| {
                (left.1.last_reset_ns, left.0).cmp(&(right.1.last_reset_ns, right.0))
            })
            .map(|(run, _)| run.clone());
        match oldest {
            Some(run) => {
                runs.remove(&run);
                Ok(Some(RunKey::new(&key.tenant, &run)))
            }
            None => Err(Error::Conflict(format!(
                "this tenant already has {MAX_CONTROLLED_RUNS_PER_TENANT} runs under a kill \
                 switch; reset one before tripping or resetting another run"
            ))),
        }
    }

    /// Brings every tenant back within [`MAX_CONTROLLED_RUNS_PER_TENANT`]
    /// after a replay, dropping the controls of runs reset longest ago; kill
    /// switches are kept whatever the count.
    fn trim_controls(&mut self) {
        for runs in self.controls.values_mut() {
            if runs.len() <= MAX_CONTROLLED_RUNS_PER_TENANT {
                continue;
            }
            let mut spent: Vec<(Option<u64>, String)> = runs
                .iter()
                .filter(|(_, controls)| controls.manual.is_none())
                .map(|(run, controls)| (controls.last_reset_ns, run.clone()))
                .collect();
            spent.sort();
            let excess = runs.len() - MAX_CONTROLLED_RUNS_PER_TENANT;
            for (_, run) in spent.into_iter().take(excess) {
                runs.remove(&run);
            }
        }
    }

    /// What a rebuild of `key` replays against, as of now.
    fn replay_inputs(&self, key: &RunKey) -> ReplayInputs {
        let controls = self.controls_of(key);
        ReplayInputs {
            reset_at: controls.and_then(|controls| controls.last_reset_ns),
            grant: controls.and_then(|controls| controls.grant),
            policies: self.tenant_policies(&key.tenant).to_vec(),
        }
    }

    /// The durable state as the records that reproduce it, for a rewrite:
    /// every policy, then per run its last reset (carrying the merged grant)
    /// before its kill switch — a kill switch is only ever active AFTER the
    /// last reset, so that order replays to the same controls.
    fn records(&self) -> Vec<Record> {
        let mut policies: Vec<&FusePolicy> = self.policies.values().flatten().collect();
        policies.sort_by(|left, right| {
            (left.created_at_ns, &left.tenant, &left.name).cmp(&(
                right.created_at_ns,
                &right.tenant,
                &right.name,
            ))
        });
        let mut records: Vec<Record> = policies
            .into_iter()
            .map(|policy| Record::Policy {
                policy: policy.clone(),
            })
            .collect();
        let mut controls: Vec<(&String, &String, &Controls)> = self
            .controls
            .iter()
            .flat_map(|(tenant, runs)| {
                runs.iter()
                    .map(move |(run, controls)| (tenant, run, controls))
            })
            .collect();
        controls.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        for (tenant, run, controls) in controls {
            if let Some(at_ns) = controls.last_reset_ns {
                records.push(Record::Reset {
                    tenant: tenant.clone(),
                    run: run.clone(),
                    grant: controls.grant,
                    at_ns,
                });
            }
            if let Some((at_ns, reason)) = &controls.manual {
                records.push(Record::Trip {
                    tenant: tenant.clone(),
                    run: run.clone(),
                    reason: reason.clone(),
                    at_ns: *at_ns,
                });
            }
        }
        records
    }

    /// The class a live run is listed under: what [`Self::judge`] would
    /// answer as of its last settle, without judging it.
    fn class_of(&self, key: &RunKey, run: &RunState) -> Verdict {
        let manual = self
            .controls_of(key)
            .is_some_and(|controls| controls.manual.is_some());
        if manual || run.trip.is_some() {
            Verdict::Stop
        } else if run.warn_level > 0 {
            Verdict::Warn
        } else {
            Verdict::Continue
        }
    }

    /// Re-files a live run in the listing index if its class changed.
    fn reclass(&mut self, key: &RunKey) {
        let Some((shared, run)) = self.runs.get_key_value(key) else {
            return;
        };
        let class = self.class_of(key, run);
        if class == run.class {
            return;
        }
        let (shared, tick, previous) = (Arc::clone(shared), run.tick, run.class);
        self.listing.remove(key, tick, previous);
        self.listing.insert(&shared, tick, class);
        if let Some(run) = self.runs.get_mut(key) {
            run.class = class;
        }
    }

    /// Adds a live run at `tick`, filed under its class.
    fn admit_live(&mut self, key: Arc<RunKey>, mut run: RunState, tick: u64, now: u64) {
        run.tick = tick;
        run.last_seen_ns = now;
        run.class = self.class_of(&key, &run);
        self.tracked += run.tracked();
        self.listing.insert(&key, tick, run.class);
        self.order.insert(tick, Arc::clone(&key));
        self.runs.insert(key, run);
    }

    /// Removes a live run and its share of the budget, returning it.
    fn take_live(&mut self, key: &RunKey) -> Option<(Arc<RunKey>, RunState)> {
        let (shared, run) = self.runs.remove_entry(key)?;
        self.order.remove(&run.tick);
        self.listing.remove(key, run.tick, run.class);
        self.tracked = self.tracked.saturating_sub(run.tracked());
        Some((shared, run))
    }

    /// Drops one live run.
    fn remove(&mut self, key: &RunKey) {
        self.take_live(key);
    }

    /// Files a stopped run as parked.
    fn park(&mut self, key: Arc<RunKey>, parked: Parked) {
        self.listing.insert(&key, parked.tick, Verdict::Stop);
        self.parked_order.insert(parked.tick, Arc::clone(&key));
        self.parked.insert(key, parked);
    }

    /// Takes a parked run out of the parked set.
    fn unpark(&mut self, key: &RunKey) -> Option<Parked> {
        let parked = self.parked.remove(key)?;
        self.parked_order.remove(&parked.tick);
        self.listing.remove(key, parked.tick, Verdict::Stop);
        Some(parked)
    }

    /// Drops the oldest parked runs beyond `max`.
    fn trim_parked(&mut self, max: usize) {
        while self.parked.len() > max.max(1) {
            let Some(key) = self.parked_order.values().next().cloned() else {
                break;
            };
            self.unpark(&key);
        }
    }

    /// The live entry for `key`, created empty (and incomplete) when absent,
    /// and moved to the recent end of the eviction order. A parked run comes
    /// back with its stop and its policy's service; its usage is followed
    /// from here until it is rebuilt.
    fn entry(&mut self, key: &RunKey, now: u64) {
        let tick = self.bump();
        if let Some(run) = self.runs.get_mut(key) {
            let (previous, class) = (run.tick, run.class);
            run.tick = tick;
            run.last_seen_ns = now;
            if let Some(shared) = self.order.remove(&previous) {
                self.order.insert(tick, shared);
            }
            self.listing.retick(key, previous, tick, class);
            return;
        }
        let mut run = RunState::new(false);
        if let Some(parked) = self.unpark(key) {
            run.trip = parked.trip;
            run.service = parked.service;
        }
        self.admit_live(Arc::new(key.clone()), run, tick, now);
    }

    /// Installs a rebuilt run in place of whatever was live or parked for it.
    fn install(&mut self, key: &RunKey, mut run: RunState, now: u64) {
        let tick = self.bump();
        // What was announced live stands: a warn already sent stays sent,
        // and a stop observed live — or parked — stays sticky with the time
        // and reasons it tripped with. A rebuild refines usage; it does not
        // un-blow a fuse, whatever the policy says now.
        let mut carried = None;
        if let Some((_, previous)) = self.take_live(key) {
            run.warn_level = run.warn_level.max(previous.warn_level);
            carried = previous.trip;
        }
        if let Some(parked) = self.unpark(key) {
            carried = carried.or(parked.trip);
        }
        if carried.is_some() {
            run.trip = carried;
        }
        self.admit_live(Arc::new(key.clone()), run, tick, now);
    }

    /// Judges `key` without changing anything: the manual stop, then the
    /// sticky policy stop, then the limits against usage (plus `next`).
    fn judge(
        &self,
        config: &FuseConfig,
        key: &RunKey,
        next: Option<&NextStep>,
        now: u64,
    ) -> Judgement {
        let empty = RunState::new(true);
        let live = self.runs.get(key);
        let parked = match live {
            Some(_) => None,
            None => self.parked.get(key),
        };
        let run = live.unwrap_or(&empty);
        let controls = self.controls_of(key);
        let service = run
            .service
            .as_deref()
            .or_else(|| parked.and_then(|parked| parked.service.as_deref()))
            .unwrap_or("");
        let resolved = policy::resolve(
            self.tenant_policies(&key.tenant),
            config,
            &key.tenant,
            service,
        );
        let grant = controls.and_then(|controls| controls.grant);
        let limits = match &grant {
            Some(grant) => resolved.limits.overlaid(grant),
            None => resolved.limits,
        };
        let mut judged = Judgement {
            policy: resolved.name.clone(),
            limits,
            grant,
            ..Judgement::default()
        };
        if let Some((at_ns, reason)) = controls.and_then(|controls| controls.manual.as_ref()) {
            judged.verdict = Verdict::Stop;
            judged.reasons = vec![if reason.is_empty() {
                "manually tripped".to_owned()
            } else {
                format!("manually tripped: {reason}")
            }];
            judged.tripped_at_ns = Some(*at_ns);
            judged.tripped_by = Some(TrippedBy::Manual);
            return judged;
        }
        let trip = run
            .trip
            .as_ref()
            .or_else(|| parked.and_then(|parked| parked.trip.as_ref()));
        if let Some(trip) = trip {
            judged.verdict = Verdict::Stop;
            judged.reasons = trip.reasons.clone();
            judged.tripped_at_ns = Some(trip.at_ns);
            judged.tripped_by = Some(TrippedBy::Policy);
            judged.evidence = trip.evidence.clone();
            return judged;
        }
        let assessment = run.assess(&limits, next);
        if resolved.action == Action::Stop && !assessment.reached.is_empty() {
            judged.verdict = Verdict::Stop;
            judged.evidence = run.trip_evidence(&assessment);
            judged.reasons = assessment.reached.clone();
            // A hypothetical step never trips anything: only recorded usage
            // makes a stop sticky, so only it reports who tripped it.
            if next.is_none() {
                judged.tripped_by = Some(TrippedBy::Policy);
                judged.tripped_at_ns = Some(now);
                judged.new_trip = Some(Trip {
                    at_ns: now,
                    reasons: assessment.reached,
                    evidence: judged.evidence.clone(),
                });
            }
            return judged;
        }
        if state::warns(&assessment, &resolved) {
            judged.verdict = Verdict::Warn;
            judged.warn_level = if assessment.reached.is_empty() { 1 } else { 2 };
            judged.reasons = assessment.reached;
            judged.reasons.extend(assessment.approaching);
            judged.evidence = assessment.evidence;
        }
        judged
    }

    /// Evicts idle live runs, then least-recently-active ones until both
    /// bounds hold, with no exceptions but `keep` — the run a caller is
    /// answering about. An evicted run that is stopped is parked rather than
    /// dropped, and parked runs beyond `max_runs` are dropped oldest first.
    fn evict(&mut self, now: u64, max_runs: usize, max_tracked: usize, keep: Option<&RunKey>) {
        let idle_before = now.saturating_sub(IDLE_NS);
        let mut victims = Vec::new();
        let mut runs = self.runs.len();
        let mut tracked = self.tracked;
        for key in self.order.values() {
            let Some(run) = self.runs.get(&**key) else {
                continue;
            };
            let over = runs > max_runs.max(1) || tracked > max_tracked;
            if !over && run.last_seen_ns >= idle_before {
                break;
            }
            if keep == Some(&**key) {
                continue;
            }
            runs -= 1;
            tracked = tracked.saturating_sub(run.tracked());
            victims.push(Arc::clone(key));
        }
        for key in victims {
            let Some((shared, run)) = self.take_live(&key) else {
                continue;
            };
            if run.class == Verdict::Stop {
                let parked = Parked {
                    tick: run.tick,
                    usage: run.usage(),
                    service: run.service,
                    trip: run.trip,
                };
                self.park(shared, parked);
            }
        }
        self.trim_parked(max_runs);
    }

    fn doc(&self, key: &RunKey, judged: Judgement, source: Source) -> VerdictDoc {
        let usage = match self.runs.get(key) {
            Some(run) => run.usage(),
            None => self
                .parked
                .get(key)
                .map(|parked| parked.usage.clone())
                .unwrap_or_default(),
        };
        VerdictDoc {
            run: key.run.clone(),
            tenant: key.tenant.clone(),
            verdict: judged.verdict,
            reasons: judged.reasons,
            policy: judged.policy,
            usage,
            limits: judged.limits,
            grant: judged.grant,
            tripped_at_ns: judged.tripped_at_ns,
            tripped_by: judged.tripped_by,
            evidence: judged.evidence,
            source,
        }
    }

    /// Ends one rebuild's registration, returning the observations captured
    /// while it read the store.
    fn finish_rebuild(&mut self, key: &RunKey) -> Vec<Observation> {
        let Some((inflight, pending)) = self.rebuilding.get_mut(key) else {
            return Vec::new();
        };
        *inflight = inflight.saturating_sub(1);
        if *inflight > 0 {
            return pending.clone();
        }
        self.rebuilding
            .remove(key)
            .map(|(_, pending)| pending)
            .unwrap_or_default()
    }
}

/// Refuses a run id no span could carry, or one too long to hold.
fn validate_run(run: &str) -> Result<()> {
    if run.is_empty() {
        return Err(Error::Invalid("run id is empty".to_owned()));
    }
    if run.len() > MAX_RUN_ID_BYTES {
        return Err(Error::Invalid(format!(
            "run id must be at most {MAX_RUN_ID_BYTES} bytes (got {})",
            run.len()
        )));
    }
    if semconv::run_trace_id(run) == Some("") {
        return Err(Error::Invalid(format!(
            "run id `{run}` names no trace; expected `{}<trace_id>`",
            semconv::TRACE_RUN_PREFIX
        )));
    }
    Ok(())
}

fn validate_tenant(tenant: &str) -> Result<()> {
    if tenant.is_empty() || crate::valid_tenant(tenant) {
        Ok(())
    } else {
        Err(Error::Invalid(
            "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes".to_owned(),
        ))
    }
}

/// A fresh policy id: `fp_` and 16 hex characters, unique per process and
/// across restarts (the clock and a process-wide counter both feed it).
fn policy_id(tenant: &str, name: &str, now: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("{tenant}\u{0}{name}\u{0}{now}\u{0}{serial}");
    let digest = crate::hash::hash128(material.as_bytes());
    let hex: String = digest.as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("fp_{hex}")
}

fn unix_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

impl Store {
    fn guard(&self) -> Result<&Guard> {
        self.guard.as_ref().ok_or_else(|| {
            Error::Conflict(
                "run fuses are disabled on this store (traza-server --no-fuses)".to_owned(),
            )
        })
    }

    /// Whether this store observes runs: `false` under `--no-fuses`
    /// ([`FuseConfig::enabled`]), when every fuse surface refuses.
    pub fn fuses_enabled(&self) -> bool {
        self.guard.is_some()
    }

    /// Feeds an acknowledged batch to the run guard; the signals become
    /// [`crate::Admission::signals`]. Empty when fuses are disabled.
    pub(crate) fn observe_fuses(&self, spans: &[Arc<Span>]) -> Vec<Signal> {
        match &self.guard {
            Some(guard) if !spans.is_empty() => guard.observe(self, spans),
            _ => Vec::new(),
        }
    }

    /// The verdict document for `run` within `tenant` (empty: the default
    /// tenant).
    ///
    /// The first question about a run rebuilds it from the store, under the
    /// query deadline, and later ones are answered from memory; a run no span
    /// carries is a fresh run with zero usage — `continue`, since a fresh run
    /// is allowed to start — and is answered without holding any state for
    /// it. If a rebuild fails (an exhausted deadline), a run with live state
    /// is answered from it, marked `live`, and the rebuild is retried on the
    /// next query. A rebuild an erasure overlapped is discarded and redone;
    /// one that keeps being overlapped is a retryable [`Error::Conflict`].
    pub fn fuse_verdict(&self, tenant: &str, run: &str) -> Result<VerdictDoc> {
        self.fuse_answer(tenant, run, None)
    }

    /// [`Self::fuse_verdict`] for a step about to happen: `next` is added to
    /// the run's usage hypothetically, so a step that WOULD go past a stop
    /// limit answers `stop`. This is the pre-call gate. Nothing about the
    /// run changes; `usage` in the answer is what the run has recorded.
    pub fn fuse_check(&self, tenant: &str, run: &str, next: &NextStep) -> Result<VerdictDoc> {
        next.validate()?;
        self.fuse_answer(tenant, run, Some(next))
    }

    fn fuse_answer(&self, tenant: &str, run: &str, next: Option<&NextStep>) -> Result<VerdictDoc> {
        let guard = self.guard()?;
        validate_tenant(tenant)?;
        validate_run(run)?;
        let key = RunKey::new(tenant, run);
        for _ in 0..REBUILD_ATTEMPTS {
            let now = unix_now_ns();
            {
                let mut state = guard.lock_state()?;
                if state.runs.get(&key).is_some_and(|live| live.complete) {
                    state.entry(&key, now);
                    guard.settle(&mut state, &key, now);
                    let judged = state.judge(&guard.config, &key, next, now);
                    return Ok(state.doc(&key, judged, Source::Live));
                }
            }
            let read = self.fuse_rebuild_read(guard, &key)?;
            if let Some(doc) = self.fuse_rebuild_install(guard, &key, read, next, now)? {
                return Ok(doc);
            }
        }
        Err(Error::Conflict(
            "erasures kept overlapping this run's rebuild; retry once they settle".to_owned(),
        ))
    }

    /// A rebuild's first half: registers it, then reads and replays the
    /// run's stored spans with no guard lock held — a rebuild of a long run
    /// must not stall every ingest observer. Observations that land
    /// meanwhile are captured for the run and replayed over the result by
    /// [`Self::fuse_rebuild_install`], so nothing admitted during the read is
    /// lost or counted twice.
    fn fuse_rebuild_read(&self, guard: &Guard, key: &RunKey) -> Result<RebuildRead> {
        let epoch = {
            let mut state = guard.lock_state()?;
            state.rebuilding.entry(key.clone()).or_default().0 += 1;
            state.erasure_epoch
        };
        // The deadline covers extraction too: digesting 100,000 requests is
        // work the budget exists to bound, not a free tail after the read.
        let deadline = self.query_deadline(Instant::now());
        let replayed = self
            .run_spans(Some(&key.tenant), &key.run, REBUILD_MAX_SPANS, deadline)
            .and_then(|stored| {
                let mut observations = Vec::with_capacity(stored.spans.len());
                for (index, span) in stored.spans.iter().enumerate() {
                    if index % DEADLINE_STRIDE == 0 {
                        Deadline::check(deadline, 0)?;
                    }
                    observations.push(Observation::of(self, span));
                }
                let inputs = guard.lock_state()?.replay_inputs(key);
                let rebuilt = inputs.replay(&guard.config, &key.tenant, &observations);
                Ok((observations, inputs, rebuilt))
            });
        Ok(RebuildRead { epoch, replayed })
    }

    /// A rebuild's second half: installs what was read and answers. `None`
    /// when an erasure ran since the read began — what was read may be what
    /// it removed, so it is discarded, never installed, and the caller reads
    /// again. The answer is judged BEFORE eviction, and eviction never takes
    /// the run being answered.
    fn fuse_rebuild_install(
        &self,
        guard: &Guard,
        key: &RunKey,
        read: RebuildRead,
        next: Option<&NextStep>,
        now: u64,
    ) -> Result<Option<VerdictDoc>> {
        let mut state = guard.lock_state()?;
        let pending = state.finish_rebuild(key);
        if state.erasure_epoch != read.epoch {
            return Ok(None);
        }
        let (observations, inputs, mut rebuilt) = match read.replayed {
            Ok(replayed) => replayed,
            Err(error) => {
                // What memory holds still answers: live usage, or a stop
                // that needs no usage to stand — parked, or a kill switch. A
                // stopped run whose history is too long to read in time must
                // still answer `stop`, not an error a hook may fail open on.
                let manual = state
                    .controls_of(key)
                    .is_some_and(|controls| controls.manual.is_some());
                if state.runs.contains_key(key) {
                    state.entry(key, now);
                } else if !state.parked.contains_key(key) && !manual {
                    return Err(error);
                }
                let judged = state.judge(&guard.config, key, next, now);
                return Ok(Some(state.doc(key, judged, Source::Live)));
            }
        };
        if state.runs.get(key).is_some_and(|live| live.complete) {
            // A concurrent rebuild installed first; its answer stands.
            state.entry(key, now);
        } else {
            // A reset or a policy edit that landed during the replay changes
            // what it must say. Rare, so the replay is simply redone.
            let current = state.replay_inputs(key);
            if current != inputs {
                rebuilt = current.replay(&guard.config, &key.tenant, &observations);
            }
            for observation in &pending {
                rebuilt.absorb(observation, false);
            }
            // Nothing stored, nothing arriving, nothing held: a fresh run,
            // answered without installing anything. A question about a run
            // that does not exist — any read-only caller can ask one — must
            // not cost memory, or distinct made-up ids would be a leak.
            let fresh = observations.is_empty()
                && pending.is_empty()
                && !state.runs.contains_key(key)
                && !state.parked.contains_key(key)
                && state.controls_of(key).is_none();
            if !fresh {
                state.install(key, rebuilt, now);
                guard.settle(&mut state, key, now);
            }
        }
        let judged = state.judge(&guard.config, key, next, now);
        let doc = state.doc(key, judged, Source::Rebuilt);
        state.evict(now, guard.config.max_runs, MAX_TRACKED_SPANS, Some(key));
        Ok(Some(doc))
    }

    /// Runs the guard holds — live ones, and stopped ones parked out of the
    /// live set — most recently active first, narrowed to `tenant` when
    /// given and to `filter`, at most `limit` of them.
    ///
    /// Answered from memory alone — a listing never rebuilds, so a run this
    /// process has followed only partly lists with the usage it has seen,
    /// and a parked run with its usage as last seen live. Ask for the run
    /// itself ([`Self::fuse_verdict`]) for the whole of it. The runs are
    /// found through an index by tenant and verdict, so the state lock is
    /// held for work proportional to `limit`, not to how many runs exist.
    pub fn fuse_runs(
        &self,
        tenant: Option<&str>,
        filter: RunListFilter,
        limit: usize,
    ) -> Result<Vec<VerdictDoc>> {
        let guard = self.guard()?;
        let now = unix_now_ns();
        let state = guard.lock_state()?;
        let keys = state.listing.newest(tenant, filter.classes(), limit);
        let docs = keys
            .iter()
            .map(|key| {
                let judged = state.judge(&guard.config, key, None, now);
                state.doc(key, judged, Source::Live)
            })
            .collect();
        Ok(docs)
    }

    /// The kill switch: stops `run` until it is reset, durably, whatever its
    /// usage. Returns the run's verdict document afterwards.
    pub fn fuse_trip(&self, tenant: &str, run: &str, reason: &str) -> Result<VerdictDoc> {
        let guard = self.guard()?;
        validate_tenant(tenant)?;
        validate_run(run)?;
        if reason.len() > MAX_REASON_BYTES {
            return Err(Error::Invalid(format!(
                "reason must be at most {MAX_REASON_BYTES} bytes"
            )));
        }
        let key = RunKey::new(tenant, run);
        let now = unix_now_ns();
        let record = Record::Trip {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            reason: reason.to_owned(),
            at_ns: now,
        };
        {
            let mut log = guard.lock_log()?;
            let dropped = guard.lock_state()?.room_for_control(&key)?;
            log.append(&record)?;
            let mut state = guard.lock_state()?;
            state.apply(&record);
            state.entry(&key, now);
            state.reclass(&key);
            if let Some(dropped) = dropped {
                guard.settle(&mut state, &dropped, now);
            }
            guard.trips.fetch_add(1, Ordering::Relaxed);
            let judged = state.judge(&guard.config, &key, None, now);
            guard.announce(&key, EventKind::ManualStop, &judged, now);
            guard.compact_if_due(&mut log, state)?;
        }
        self.fuse_verdict(tenant, run)
    }

    /// Clears `run`'s stop, durably: the kill switch, a sticky policy stop,
    /// the error streak and the repeat table. `grant` raises the run's limits
    /// to the absolute values it names, merged over earlier grants.
    ///
    /// Cumulative usage is untouched, so a run reset without a grant while
    /// still over a cumulative limit stops again at once — the answer says
    /// so. Returns the run's verdict document afterwards.
    pub fn fuse_reset(&self, tenant: &str, run: &str, grant: Option<Limits>) -> Result<VerdictDoc> {
        let guard = self.guard()?;
        validate_tenant(tenant)?;
        validate_run(run)?;
        if let Some(grant) = &grant {
            grant.validate("grant")?;
        }
        let key = RunKey::new(tenant, run);
        let now = unix_now_ns();
        let record = Record::Reset {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            grant: grant.filter(|grant| !grant.is_empty()),
            at_ns: now,
        };
        {
            let mut log = guard.lock_log()?;
            let dropped = guard.lock_state()?.room_for_control(&key)?;
            log.append(&record)?;
            let mut state = guard.lock_state()?;
            state.apply(&record);
            // A parked stop is what a reset clears; the run's spans are
            // re-judged below, and by the rebuild that answers.
            state.unpark(&key);
            if let Some(live) = state.runs.get_mut(&key) {
                live.reset();
            }
            state.entry(&key, now);
            let mut judged = state.judge(&guard.config, &key, None, now);
            judged.reasons = vec![match &grant {
                Some(grant) if !grant.is_empty() => {
                    format!("reset with grant {}", serde_json::to_string(grant)?)
                }
                _ => "reset".to_owned(),
            }];
            guard.announce(&key, EventKind::Reset, &judged, now);
            guard.settle(&mut state, &key, now);
            if let Some(dropped) = dropped {
                guard.settle(&mut state, &dropped, now);
            }
            guard.compact_if_due(&mut log, state)?;
        }
        self.fuse_verdict(tenant, run)
    }

    /// Creates the policy `spec` names, or replaces the tenant's policy of
    /// that name (keeping its id and creation time). Live runs of the tenant
    /// are re-judged at once. A tenant holds at most
    /// [`MAX_POLICIES_PER_TENANT`]; creating one more is a conflict.
    pub fn put_fuse_policy(&self, spec: PolicySpec) -> Result<FusePolicy> {
        let guard = self.guard()?;
        spec.validate()?;
        let now = unix_now_ns();
        let mut log = guard.lock_log()?;
        let (existing, count) = {
            let state = guard.lock_state()?;
            let policies = state.tenant_policies(&spec.tenant);
            let existing = policies
                .iter()
                .find(|policy| policy.name == spec.name)
                .map(|policy| (policy.id.clone(), policy.created_at_ns));
            (existing, policies.len())
        };
        if existing.is_none() && count >= MAX_POLICIES_PER_TENANT {
            return Err(Error::Conflict(format!(
                "this tenant already has {MAX_POLICIES_PER_TENANT} fuse policies; delete one \
                 before creating another"
            )));
        }
        let (id, created_at_ns) =
            existing.unwrap_or_else(|| (policy_id(&spec.tenant, &spec.name, now), now));
        let policy = FusePolicy {
            id,
            tenant: spec.tenant,
            name: spec.name,
            matches: spec.matches,
            limits: spec.limits,
            action: spec.action,
            created_at_ns,
        };
        let record = Record::Policy {
            policy: policy.clone(),
        };
        log.append(&record)?;
        guard.lock_state()?.apply(&record);
        guard.settle_tenant(&policy.tenant, now)?;
        let state = guard.lock_state()?;
        guard.compact_if_due(&mut log, state)?;
        Ok(policy)
    }

    /// Stored policies, by tenant then name; narrowed to `tenant` when given.
    pub fn fuse_policies(&self, tenant: Option<&str>) -> Result<Vec<FusePolicy>> {
        let guard = self.guard()?;
        let state = guard.lock_state()?;
        let mut policies: Vec<FusePolicy> = match tenant {
            Some(tenant) => state.tenant_policies(tenant).to_vec(),
            None => state.policies.values().flatten().cloned().collect(),
        };
        policies
            .sort_by(|left, right| (&left.tenant, &left.name).cmp(&(&right.tenant, &right.name)));
        Ok(policies)
    }

    /// The server-wide default policy's limits and action — the policy a
    /// run gets, under the name [`GLOBAL_POLICY_NAME`], when its tenant has
    /// no matching or `default` policy.
    pub fn fuse_global_policy(&self) -> Result<(Limits, Action)> {
        let guard = self.guard()?;
        Ok((guard.config.limits, guard.config.action))
    }

    /// Deletes policy `id`, durably, when it exists and (given `tenant`)
    /// belongs to that tenant; `Ok(None)` otherwise. Live runs of its tenant
    /// are re-judged at once.
    pub fn delete_fuse_policy(&self, tenant: Option<&str>, id: &str) -> Result<Option<FusePolicy>> {
        let guard = self.guard()?;
        let now = unix_now_ns();
        let mut log = guard.lock_log()?;
        let found = {
            let state = guard.lock_state()?;
            let mut candidates: Box<dyn Iterator<Item = &FusePolicy>> = match tenant {
                Some(tenant) => Box::new(state.tenant_policies(tenant).iter()),
                None => Box::new(state.policies.values().flatten()),
            };
            candidates.find(|policy| policy.id == id).cloned()
        };
        let Some(policy) = found else {
            return Ok(None);
        };
        let record = Record::DeletePolicy {
            id: policy.id.clone(),
            at_ns: now,
        };
        log.append(&record)?;
        guard.lock_state()?.apply(&record);
        guard.settle_tenant(&policy.tenant, now)?;
        let state = guard.lock_state()?;
        guard.compact_if_due(&mut log, state)?;
        Ok(Some(policy))
    }

    /// Fuse events after cursor `after` (all retained ones when `None`),
    /// narrowed to `tenant` when given, waiting up to `wait` for one to
    /// arrive. See [`EventPage`] for the cursor and gap rules.
    pub fn fuse_events(
        &self,
        tenant: Option<&str>,
        after: Option<&str>,
        wait: Duration,
    ) -> Result<EventPage> {
        let guard = self.guard()?;
        let keep = |event: &FuseEvent| tenant.map_or(true, |tenant| event.tenant == tenant);
        guard
            .events
            .read(after, wait.min(Duration::from_millis(MAX_WAIT_MS)), &keep)
            .map_err(Error::Invalid)
    }

    /// Removes what the run guard holds about an erasure subject, for one
    /// purge pass that removed the spans `keys` names.
    ///
    /// The guard keeps no span text, but it does keep identifiers and
    /// telemetry-derived strings: live runs hold span ids as evidence and
    /// fingerprints that carry tool and model names, parked runs hold their
    /// stop's evidence and reasons, the event feed holds run ids and
    /// reasons, and `fuses.jsonl` holds run ids — which are session ids —
    /// with kill-switch reasons. So: every live run any purged span
    /// contributed to is dropped (asked about again, it is rebuilt from what
    /// the store still holds); a parked run is dropped when it is named, its
    /// evidence names a purged span, or it is a purged trace's run; a tenant
    /// subject drops the tenant's runs, events, policies and controls
    /// wholesale; a session or trace subject drops that run's events and
    /// controls. Every rebuild in flight discards what it read; an ingest
    /// observer still to run for a batch acknowledged before the erasure
    /// began skips what the pending mask covers (`Guard::observe`). For a
    /// tenant, session or trace subject `fuses.jsonl` is rewritten from what
    /// remains, whether or not anything in memory changed: old records can
    /// name the subject when nothing in memory does (a policy deleted
    /// earlier, a run's controls dropped to make room). Idempotent, so a
    /// resumed erasure — and the confirm pass — may run it again.
    pub(crate) fn forget_fuse_subject(
        &self,
        subject: &erasure::Subject,
        keys: &HashSet<(String, String, String)>,
    ) -> Result<()> {
        let Some(guard) = &self.guard else {
            return Ok(());
        };
        let tenant = subject.tenant();
        let whole_tenant = matches!(subject, erasure::Subject::Tenant { .. });
        let named_run = match subject {
            erasure::Subject::Session { session_id, .. } => Some(session_id.clone()),
            erasure::Subject::Trace { trace_id, .. } => {
                Some(format!("{}{trace_id}", semconv::TRACE_RUN_PREFIX))
            }
            _ => None,
        };
        let names = |key_tenant: &str, run: &str| {
            tenant == Some(key_tenant) && (whole_tenant || named_run.as_deref() == Some(run))
        };
        let mut purged: HashMap<&str, HashSet<u64>> = HashMap::new();
        let mut purged_traces: HashSet<(&str, &str)> = HashSet::new();
        for (key_tenant, trace_id, span_id) in keys {
            purged
                .entry(key_tenant.as_str())
                .or_default()
                .insert(state::span_key(trace_id, span_id));
            purged_traces.insert((key_tenant.as_str(), trace_id.as_str()));
        }
        let parked_touched = |key: &RunKey, parked: &Parked| {
            let named_span = purged.get(key.tenant.as_str()).is_some_and(|spans| {
                parked
                    .span_refs()
                    .any(|span| spans.contains(&state::span_key(&span.trace_id, &span.span_id)))
            });
            let purged_trace = semconv::run_trace_id(&key.run)
                .is_some_and(|trace| purged_traces.contains(&(key.tenant.as_str(), trace)));
            named_span || purged_trace
        };

        let mut log = guard.lock_log()?;
        let records = {
            let mut state = guard.lock_state()?;
            // A rebuild that read the store before this pass may hold what
            // it removed: the epoch makes it discard the read, and what it
            // captured meanwhile goes too (it reads again).
            state.erasure_epoch += 1;
            for (_, pending) in state.rebuilding.values_mut() {
                pending.clear();
            }
            let doomed: Vec<RunKey> = state
                .runs
                .iter()
                .filter(|(key, run)| {
                    names(&key.tenant, &key.run)
                        || purged
                            .get(key.tenant.as_str())
                            .is_some_and(|spans| run.holds_any(spans))
                })
                .map(|(key, _)| (**key).clone())
                .collect();
            for key in &doomed {
                state.remove(key);
            }
            let unparked: Vec<RunKey> = state
                .parked
                .iter()
                .filter(|(key, parked)| names(&key.tenant, &key.run) || parked_touched(key, parked))
                .map(|(key, _)| (**key).clone())
                .collect();
            for key in &unparked {
                state.unpark(key);
            }
            guard
                .events
                .retain(&|event: &FuseEvent| !names(&event.tenant, &event.run));
            match (tenant, &named_run) {
                (Some(tenant), _) if whole_tenant => {
                    state.policies.remove(tenant);
                    state.controls.remove(tenant);
                }
                (Some(tenant), Some(run)) => {
                    if let Some(runs) = state.controls.get_mut(tenant) {
                        runs.remove(run);
                        if runs.is_empty() {
                            state.controls.remove(tenant);
                        }
                    }
                }
                _ => {}
            }
            (whole_tenant || named_run.is_some()).then(|| state.records())
        };
        // Written with the state lock released — ingest observers never wait
        // on this fsync — but under the log lock, which every durable
        // mutation takes first, so nothing can interleave with the rewrite.
        if let Some(records) = records {
            log.rewrite(&records)?;
        }
        drop(log);
        #[cfg(test)]
        if let Some(hook) = guard
            .after_forget
            .lock()
            .ok()
            .and_then(|mut hook| hook.take())
        {
            hook(self);
        }
        Ok(())
    }

    /// Appends the fuse counters in Prometheus text format: trips (policy and
    /// manual), warnings, and the live-run gauge. Nothing when fuses are
    /// disabled.
    pub fn render_fuse_metrics(&self, into: &mut String) {
        use std::fmt::Write as _;
        let Some(guard) = &self.guard else {
            return;
        };
        let live = guard.state.lock().map_or(0, |state| state.runs.len());
        let _ = writeln!(into, "# TYPE traza_fuse_trips_total counter");
        let _ = writeln!(
            into,
            "traza_fuse_trips_total {}",
            guard.trips.load(Ordering::Relaxed)
        );
        let _ = writeln!(into, "# TYPE traza_fuse_warnings_total counter");
        let _ = writeln!(
            into,
            "traza_fuse_warnings_total {}",
            guard.warnings.load(Ordering::Relaxed)
        );
        let _ = writeln!(into, "# TYPE traza_fuse_live_runs gauge");
        let _ = writeln!(into, "traza_fuse_live_runs {live}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    #[test]
    fn verdicts_order_by_severity_and_serialize_lowercase() {
        assert!(Verdict::Continue < Verdict::Warn && Verdict::Warn < Verdict::Stop);
        assert_eq!(
            [Verdict::Warn, Verdict::Stop, Verdict::Continue]
                .into_iter()
                .max(),
            Some(Verdict::Stop)
        );
        for verdict in [Verdict::Continue, Verdict::Warn, Verdict::Stop] {
            assert_eq!(
                serde_json::to_value(verdict).expect("serializes"),
                json!(verdict.as_str())
            );
        }
        assert_eq!(Verdict::default(), Verdict::Continue);
    }

    #[test]
    fn a_signal_has_the_documented_wire_shape() {
        let signal = Signal {
            tenant: "acme".into(),
            run: "sess-1".into(),
            verdict: Verdict::Stop,
            reasons: vec!["max_steps 50 reached".into()],
        };
        let wire = serde_json::to_value(&signal).expect("serializes");
        assert_eq!(
            wire,
            json!({"tenant": "acme", "run": "sess-1", "verdict": "stop",
                   "reasons": ["max_steps 50 reached"]})
        );
        let back: Signal = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(back, signal);
    }

    #[test]
    fn the_header_names_the_most_severe_run_and_cannot_be_split() {
        let signal = |run: &str, verdict| Signal {
            tenant: String::new(),
            run: run.into(),
            verdict,
            reasons: Vec::new(),
        };
        assert_eq!(fuse_header(&[]), None);
        assert_eq!(
            fuse_header(&[
                signal("a", Verdict::Warn),
                signal("trace:b", Verdict::Stop),
                signal("c", Verdict::Warn),
            ])
            .as_deref(),
            Some("stop; run=trace:b")
        );
        assert_eq!(
            fuse_header(&[signal("x\r\nSet-Cookie: y;z", Verdict::Warn)]).as_deref(),
            Some("warn; run=x%0D%0ASet-Cookie:%20y%3Bz")
        );
    }

    struct TempStore {
        dir: std::path::PathBuf,
        store: Option<Store>,
    }

    impl TempStore {
        fn open(label: &str, fuses: FuseConfig) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "traza-guard-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("dir");
            let store = Self::reopen_at(&dir, fuses);
            Self {
                dir,
                store: Some(store),
            }
        }

        fn reopen_at(dir: &Path, fuses: FuseConfig) -> Store {
            Store::open(
                dir,
                crate::Config {
                    fuses,
                    ..crate::Config::default()
                },
            )
            .expect("opens")
        }

        fn store(&self) -> &Store {
            self.store.as_ref().expect("open")
        }

        fn reopen(&mut self, fuses: FuseConfig) {
            drop(self.store.take());
            self.store = Some(Self::reopen_at(&self.dir, fuses));
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            drop(self.store.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn tool_span(run: &str, id: &str, tool: &str, input: &str, start: u64) -> Span {
        let attributes = json!({
            "session.id": run,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": tool,
            "gen_ai.tool.call.arguments": input,
        });
        Span {
            trace_id: format!("t-{run}"),
            span_id: id.into(),
            tenant: String::new(),
            parent_span_id: None,
            name: tool.into(),
            start_time_ns: start,
            end_time_ns: start + 10,
            status: "ok".into(),
            service: "agent".into(),
            attributes: match attributes {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn stop_on(limits: Limits) -> FuseConfig {
        FuseConfig {
            limits,
            action: Action::Stop,
            ..FuseConfig::default()
        }
    }

    #[test]
    fn an_admission_stops_the_run_whose_steps_spent_its_budget() {
        let fixture = TempStore::open(
            "steps",
            stop_on(Limits {
                max_steps: Some(3),
                ..Limits::default()
            }),
        );
        let store = fixture.store();
        let first = store
            .ingest_batch(vec![
                tool_span("r1", "a", "search", "{\"q\":1}", 10),
                tool_span("r1", "b", "search", "{\"q\":2}", 20),
            ])
            .expect("ingests");
        assert!(
            first.signals.is_empty(),
            "2 of 3 is 66%: {:?}",
            first.signals
        );
        let second = store
            .ingest_batch(vec![tool_span("r1", "c", "fetch", "{}", 30)])
            .expect("ingests");
        assert_eq!(second.signals.len(), 1);
        assert_eq!(second.signals[0].verdict, Verdict::Stop);
        assert_eq!(second.signals[0].run, "r1");
        assert_eq!(second.signals[0].reasons, ["max_steps 3 reached: 3 steps"]);

        let doc = store.fuse_verdict("", "r1").expect("verdict");
        assert_eq!(doc.verdict, Verdict::Stop);
        assert_eq!(doc.tripped_by, Some(TrippedBy::Policy));
        assert_eq!(doc.evidence[0].span_id, "c");
        assert_eq!(doc.usage.steps, 3);
        assert_eq!(doc.policy, GLOBAL_POLICY_NAME);
        // This process followed the run from its first span, but it cannot
        // know that nothing was stored before it started: the first query
        // reads the store, and the stop it observed live stands.
        assert_eq!(doc.source, Source::Rebuilt);
        let again = store.fuse_verdict("", "r1").expect("verdict");
        assert_eq!(again.source, Source::Live);
        // An unrelated run in the same store is untouched.
        let other = store.fuse_verdict("", "r2").expect("verdict");
        assert_eq!((other.verdict, other.usage.steps), (Verdict::Continue, 0));
    }

    #[test]
    fn a_restart_rebuilds_usage_and_the_stop_from_the_store() {
        let fuses = stop_on(Limits {
            max_repeats: Some(3),
            ..Limits::default()
        });
        let mut fixture = TempStore::open("restart", fuses.clone());
        let spans: Vec<Span> = (0..4)
            .map(|index| tool_span("loop", &format!("s{index}"), "search", "same", index * 100))
            .collect();
        fixture.store().ingest_batch(spans).expect("ingests");
        fixture.reopen(fuses);
        let doc = fixture.store().fuse_verdict("", "loop").expect("verdict");
        assert_eq!(doc.source, Source::Rebuilt);
        assert_eq!(doc.verdict, Verdict::Stop);
        assert_eq!(doc.usage.steps, 4);
        let top = doc.usage.top_repeat.expect("a repeat");
        assert_eq!(top.count, 4);
        assert!(
            top.fingerprint.starts_with("tool:search#"),
            "{}",
            top.fingerprint
        );
        assert_eq!(doc.tripped_at_ns, Some(210), "the third occurrence's end");
    }

    #[test]
    fn disabled_fuses_observe_nothing_and_refuse_every_surface() {
        let fixture = TempStore::open(
            "disabled",
            FuseConfig {
                enabled: false,
                ..stop_on(Limits {
                    max_steps: Some(1),
                    ..Limits::default()
                })
            },
        );
        let store = fixture.store();
        let admission = store
            .ingest_batch(vec![tool_span("r", "a", "x", "{}", 1)])
            .expect("ingests");
        assert!(admission.signals.is_empty());
        assert!(matches!(
            store.fuse_verdict("", "r"),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            store.fuse_trip("", "r", ""),
            Err(Error::Conflict(_))
        ));
        assert!(
            !fixture.dir.join(log::LOG_NAME).exists(),
            "the log is not touched"
        );
        let mut rendered = String::new();
        store.render_fuse_metrics(&mut rendered);
        assert!(rendered.is_empty());
    }

    #[test]
    fn a_warn_policy_announces_approaching_and_then_reached() {
        let fixture = TempStore::open(
            "warnlevels",
            FuseConfig {
                limits: Limits {
                    max_steps: Some(5),
                    ..Limits::default()
                },
                ..FuseConfig::default()
            },
        );
        let store = fixture.store();
        let step = |id: &str| tool_span("w", id, "search", id, 1);
        let mut kinds = Vec::new();
        for batch in [vec!["a", "b", "c", "d"], vec!["e"], vec!["f"]] {
            let admission = store
                .ingest_batch(batch.into_iter().map(step).collect())
                .expect("ingests");
            kinds.push(admission.signals[0].reasons.clone());
        }
        assert_eq!(kinds[0], ["max_steps 5 at 80%: 4 steps"]);
        assert_eq!(
            kinds[1],
            ["max_steps 5 reached: 5 steps"],
            "warn, never stop"
        );
        let feed = store
            .fuse_events(None, None, Duration::ZERO)
            .expect("events");
        let reasons: Vec<&str> = feed
            .events
            .iter()
            .map(|event| event.reasons[0].as_str())
            .collect();
        // Entering warn, then reaching the limit; the sixth step is still at
        // the reached level and is not announced again.
        assert_eq!(
            reasons,
            [
                "max_steps 5 at 80%: 4 steps",
                "max_steps 5 reached: 5 steps"
            ]
        );
        let mut rendered = String::new();
        store.render_fuse_metrics(&mut rendered);
        assert!(
            rendered.contains("traza_fuse_warnings_total 2\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("traza_fuse_trips_total 0\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_trace_without_steps_is_not_followed_until_asked_about() {
        let fixture = TempStore::open("stepless", FuseConfig::default());
        let store = fixture.store();
        let mut http = tool_span("plain", "a", "GET /", "{}", 5);
        http.attributes.clear();
        http.end_time_ns = 905;
        store.ingest_batch(vec![http]).expect("ingests");
        let live = store
            .fuse_runs(None, RunListFilter::Active, 10)
            .expect("lists");
        assert!(live.is_empty(), "{live:?}");
        // Asked about, it is rebuilt like any other run.
        let doc = store.fuse_verdict("", "trace:t-plain").expect("verdict");
        assert_eq!((doc.source, doc.usage.duration_ns), (Source::Rebuilt, 900));
        assert_eq!(doc.usage.steps, 0);
    }

    #[test]
    fn an_erasure_reaches_live_runs_the_feed_and_the_fuse_log() {
        let mut fixture = TempStore::open(
            "erase",
            stop_on(Limits {
                max_steps: Some(2),
                ..Limits::default()
            }),
        );
        let store = fixture.store();
        let acme = |run: &str, id: &str| {
            let mut span = tool_span(run, id, "search", id, 1);
            span.tenant = "acme".into();
            span
        };
        store
            .ingest_batch(vec![
                acme("secret-session", "a"),
                acme("secret-session", "b"),
            ])
            .expect("ingests");
        store
            .ingest_batch(vec![tool_span("keep", "a", "x", "1", 1)])
            .expect("ingests");
        store
            .fuse_trip("acme", "secret-session", "customer 4417 complained")
            .expect("trips");
        store.fuse_trip("", "keep", "unrelated").expect("trips");
        store
            .put_fuse_policy(PolicySpec {
                name: "default".into(),
                tenant: "acme".into(),
                limits: Limits {
                    max_steps: Some(9),
                    ..Limits::default()
                },
                ..PolicySpec::default()
            })
            .expect("policy");

        store
            .erase(erasure::Subject::Session {
                session_id: "secret-session".into(),
                tenant: "acme".into(),
            })
            .expect("erases");
        let log = std::fs::read_to_string(fixture.dir.join(log::LOG_NAME)).expect("log");
        assert!(
            !log.contains("secret-session") && !log.contains("4417"),
            "{log}"
        );
        assert!(
            log.contains("unrelated"),
            "other runs' controls stay: {log}"
        );
        let feed = store
            .fuse_events(None, None, Duration::ZERO)
            .expect("events");
        assert!(feed
            .events
            .iter()
            .all(|event| event.run != "secret-session"));
        // Asked about again, the run is rebuilt from what the store still
        // holds: nothing.
        let doc = store
            .fuse_verdict("acme", "secret-session")
            .expect("verdict");
        assert_eq!((doc.verdict, doc.usage.steps), (Verdict::Continue, 0));
        assert!(doc.evidence.is_empty());

        // A tenant subject takes the tenant's policies too.
        store
            .erase(erasure::Subject::Tenant {
                tenant: "acme".into(),
            })
            .expect("erases");
        assert!(store.fuse_policies(Some("acme")).expect("lists").is_empty());
        fixture.reopen(stop_on(Limits::default()));
        let store = fixture.store();
        assert!(store.fuse_policies(None).expect("lists").is_empty());
        let kept = store.fuse_verdict("", "keep").expect("verdict");
        assert_eq!(
            kept.tripped_by,
            Some(TrippedBy::Manual),
            "the rewrite kept it"
        );
    }

    fn live_runs(store: &Store) -> usize {
        store
            .guard()
            .expect("guard")
            .lock_state()
            .expect("state")
            .runs
            .len()
    }

    fn listed(store: &Store, tenant: Option<&str>, filter: RunListFilter) -> Vec<String> {
        store
            .fuse_runs(tenant, filter, 100)
            .expect("lists")
            .into_iter()
            .map(|doc| doc.run)
            .collect()
    }

    #[test]
    fn eviction_keeps_the_bound_and_parks_a_kill_switch() {
        let fixture = TempStore::open(
            "evict",
            FuseConfig {
                max_runs: 2,
                ..FuseConfig::default()
            },
        );
        let store = fixture.store();
        store.fuse_trip("", "pinned", "hold").expect("trips");
        for run in ["a", "b", "c"] {
            store
                .ingest_batch(vec![tool_span(run, "s", "x", "{}", 1)])
                .expect("ingests");
        }
        assert_eq!(live_runs(store), 2, "the bound holds, kill switch or not");
        assert_eq!(
            listed(store, None, RunListFilter::Active),
            ["c", "b", "pinned"],
            "newest first; the stopped run is parked, not forgotten"
        );
        assert_eq!(listed(store, None, RunListFilter::Stop), ["pinned"]);
        let doc = store.fuse_verdict("", "pinned").expect("verdict");
        assert_eq!(doc.tripped_by, Some(TrippedBy::Manual));
        // An evicted run is not lost: it rebuilds.
        let doc = store.fuse_verdict("", "a").expect("verdict");
        assert_eq!((doc.source, doc.usage.steps), (Source::Rebuilt, 1));
    }

    #[test]
    fn stopped_runs_cannot_starve_eviction_or_open_the_gate() {
        // More kill switches than live slots: before, they were pinned yet
        // counted, so every other run was evicted at the end of every batch
        // and the check judged a run eviction had just removed.
        let fixture = TempStore::open(
            "starve",
            FuseConfig {
                max_runs: 2,
                ..stop_on(Limits {
                    max_steps: Some(3),
                    ..Limits::default()
                })
            },
        );
        let store = fixture.store();
        for run in ["k1", "k2", "k3"] {
            store.fuse_trip("", run, "hold").expect("trips");
        }
        // Usage accumulates across batches, so the third step stops the run.
        for (index, id) in ["a", "b"].into_iter().enumerate() {
            let admission = store
                .ingest_batch(vec![tool_span("r", id, "x", id, index as u64)])
                .expect("ingests");
            assert!(admission.signals.is_empty(), "{:?}", admission.signals);
        }
        let admission = store
            .ingest_batch(vec![tool_span("r", "c", "x", "c", 2)])
            .expect("ingests");
        assert_eq!(
            admission.signals.first().map(|signal| signal.verdict),
            Some(Verdict::Stop),
            "{:?}",
            admission.signals
        );

        // A run over its limit that is not live: the check rebuilds it and
        // answers from what it rebuilt — never from an empty state.
        store
            .ingest_batch(
                ["a", "b", "c"]
                    .iter()
                    .enumerate()
                    .map(|(index, id)| tool_span("q", id, "y", id, index as u64))
                    .collect(),
            )
            .expect("ingests");
        for filler in ["f1", "f2", "f3"] {
            store
                .ingest_batch(vec![tool_span(filler, "s", "z", "{}", 1)])
                .expect("ingests");
        }
        store
            .guard()
            .expect("guard")
            .lock_state()
            .expect("state")
            .evict(unix_now_ns(), 1, MAX_TRACKED_SPANS, None);
        let next = NextStep::default();
        let doc = store.fuse_check("", "q", &next).expect("checks");
        assert_eq!(doc.verdict, Verdict::Stop, "{doc:?}");
        assert_eq!(doc.usage.steps, 3);
        assert!(live_runs(store) <= 2);
        for run in ["k1", "k2", "k3"] {
            let doc = store.fuse_verdict("", run).expect("verdict");
            assert_eq!(doc.tripped_by, Some(TrippedBy::Manual), "{run}");
        }
    }

    #[test]
    fn eviction_holds_the_span_budget_and_parks_stopped_runs_without_their_marks() {
        let fixture = TempStore::open("budget", FuseConfig::default());
        let store = fixture.store();
        let observation =
            |run: &str, id: &str| Observation::of(store, &tool_span(run, id, "x", id, 1));
        let mut state = State::default();
        for (run, spans) in [("stopped", 6), ("old", 3), ("new", 2)] {
            let key = RunKey::new("", run);
            state.entry(&key, 1);
            let added = {
                let live = state.runs.get_mut(&key).expect("live");
                for index in 0..spans {
                    live.absorb(&observation(run, &format!("s{index}")), false);
                }
                live.tracked()
            };
            state.tracked += added;
        }
        let stopped = RunKey::new("", "stopped");
        state.runs.get_mut(&stopped).expect("live").trip = Some(Trip {
            at_ns: 9,
            reasons: vec!["max_steps 6 reached: 6 steps".into()],
            evidence: Vec::new(),
        });
        state.reclass(&stopped);
        assert_eq!(state.tracked, 11);

        // Four marks allowed: the stopped run goes (parked), then "old".
        state.evict(2, 100, 4, None);
        assert_eq!(state.tracked, 2);
        assert_eq!(state.runs.len(), 1);
        assert!(state.runs.contains_key(&RunKey::new("", "new")));
        let parked = state.parked.get(&stopped).expect("parked");
        assert_eq!(parked.usage.steps, 6, "usage as last seen");
        let judged = state.judge(&FuseConfig::default(), &stopped, None, 3);
        assert_eq!(judged.verdict, Verdict::Stop);
        assert_eq!(judged.reasons, ["max_steps 6 reached: 6 steps"]);

        // The run being answered is never the one evicted.
        let new = RunKey::new("", "new");
        state.evict(4, 100, 0, Some(&new));
        assert!(state.runs.contains_key(&new));
    }

    #[test]
    fn a_policy_stop_survives_eviction_and_a_raised_policy() {
        let fixture = TempStore::open(
            "sticky",
            FuseConfig {
                max_runs: 1,
                ..stop_on(Limits {
                    max_steps: Some(2),
                    ..Limits::default()
                })
            },
        );
        let store = fixture.store();
        let admission = store
            .ingest_batch(vec![
                tool_span("a", "s1", "x", "1", 1),
                tool_span("a", "s2", "x", "2", 2),
            ])
            .expect("ingests");
        assert_eq!(admission.signals[0].verdict, Verdict::Stop);
        // Another run takes the only live slot: "a" is parked.
        store
            .ingest_batch(vec![tool_span("b", "s1", "x", "1", 1)])
            .expect("ingests");
        assert_eq!(listed(store, None, RunListFilter::Stop), ["a"]);
        // Raising the limit does not lift the stop, live, parked or rebuilt.
        store
            .put_fuse_policy(PolicySpec {
                name: DEFAULT_POLICY_NAME.into(),
                limits: Limits {
                    max_steps: Some(100),
                    ..Limits::default()
                },
                action: Action::Stop,
                ..PolicySpec::default()
            })
            .expect("policy");
        // A later span of "a" — not even a step — is answered stop, though
        // the live state it lands in starts from nothing.
        let mut wrapper = tool_span("a", "w", "x", "", 0);
        wrapper.attributes.retain(|key, _| key == "session.id");
        let admission = store.ingest_batch(vec![wrapper]).expect("ingests");
        assert_eq!(admission.signals.len(), 1, "{:?}", admission.signals);
        assert_eq!(admission.signals[0].verdict, Verdict::Stop);
        assert_eq!(
            admission.signals[0].reasons,
            ["max_steps 2 reached: 2 steps"]
        );
        store
            .ingest_batch(vec![tool_span("b", "s2", "x", "2", 2)])
            .expect("ingests");
        let doc = store.fuse_verdict("", "a").expect("verdict");
        assert_eq!(
            (doc.verdict, doc.tripped_by, doc.policy.as_str()),
            (Verdict::Stop, Some(TrippedBy::Policy), DEFAULT_POLICY_NAME)
        );
        assert_eq!(doc.usage.steps, 2, "rebuilt whole");
        // Only a reset clears it.
        let doc = store.fuse_reset("", "a", None).expect("resets");
        assert_eq!(doc.verdict, Verdict::Continue, "{doc:?}");
        assert!(listed(store, None, RunListFilter::Stop).is_empty());
    }

    #[test]
    fn a_stopped_run_whose_rebuild_fails_still_answers_stop() {
        let fixture = TempStore::open(
            "failedrebuild",
            FuseConfig {
                max_runs: 2,
                ..stop_on(Limits {
                    max_steps: Some(1),
                    ..Limits::default()
                })
            },
        );
        let store = fixture.store();
        let guard = store.guard().expect("guard");
        store
            .ingest_batch(vec![tool_span("parked", "s", "x", "1", 1)])
            .expect("ingests");
        store
            .ingest_batch(vec![tool_span("other", "s", "x", "1", 1)])
            .expect("ingests");
        store.fuse_trip("", "killed", "hold").expect("trips");
        {
            // The kill-switched run out of memory altogether: only its
            // durable control is left.
            let mut state = guard.lock_state().expect("state");
            let killed = RunKey::new("", "killed");
            state.remove(&killed);
            state.unpark(&killed);
            assert!(state.parked.contains_key(&RunKey::new("", "parked")));
        }
        let failed = |run: &str| {
            let epoch = guard.lock_state().expect("state").erasure_epoch;
            let read = RebuildRead {
                epoch,
                replayed: Err(Error::DeadlineExceeded("budget spent".into())),
            };
            let key = RunKey::new("", run);
            store.fuse_rebuild_install(guard, &key, read, None, unix_now_ns())
        };
        for run in ["parked", "killed"] {
            let doc = failed(run).expect("answers").expect("not overlapped");
            assert_eq!(
                (doc.verdict, doc.source),
                (Verdict::Stop, Source::Live),
                "{run}"
            );
        }
        assert!(matches!(failed("unknown"), Err(Error::DeadlineExceeded(_))));
    }

    #[test]
    fn a_question_about_a_run_that_does_not_exist_holds_nothing() {
        let fixture = TempStore::open("fresh", FuseConfig::default());
        let store = fixture.store();
        for index in 0..50 {
            let doc = store
                .fuse_verdict("", &format!("made-up-{index}"))
                .expect("verdict");
            assert_eq!((doc.verdict, doc.usage.steps), (Verdict::Continue, 0));
            let next = NextStep {
                tool: Some("search".into()),
                ..NextStep::default()
            };
            store
                .fuse_check("", &format!("other-{index}"), &next)
                .expect("checks");
        }
        assert_eq!(live_runs(store), 0);
        assert!(listed(store, None, RunListFilter::Active).is_empty());

        // A run id too long to hold is refused, and not followed when a
        // span carries one.
        let long = "x".repeat(MAX_RUN_ID_BYTES + 1);
        match store.fuse_verdict("", &long) {
            Err(Error::Invalid(message)) => assert!(message.contains("at most 1024"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(store.fuse_verdict("", &long[..MAX_RUN_ID_BYTES]).is_ok());
        store
            .ingest_batch(vec![tool_span(&long, "s", "x", "{}", 1)])
            .expect("ingests");
        assert_eq!(live_runs(store), 0);
    }

    #[test]
    fn an_erasure_during_a_rebuild_discards_what_the_rebuild_read() {
        let fixture = TempStore::open("epoch", FuseConfig::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                tool_span("secret", "a", "search", "q", 1),
                tool_span("secret", "b", "search", "q", 2),
            ])
            .expect("ingests");
        let guard = store.guard().expect("guard");
        let key = RunKey::new("", "secret");
        // The store is read; then, before the result is installed, the run's
        // session is erased.
        let read = store.fuse_rebuild_read(guard, &key).expect("reads");
        assert!(matches!(&read.replayed, Ok((observations, _, _)) if observations.len() == 2));
        store
            .erase(erasure::Subject::Session {
                session_id: "secret".into(),
                tenant: String::new(),
            })
            .expect("erases");
        let installed = store
            .fuse_rebuild_install(guard, &key, read, None, unix_now_ns())
            .expect("installs");
        assert!(
            installed.is_none(),
            "a read the erasure overtook is discarded"
        );
        assert!(!guard.lock_state().expect("state").runs.contains_key(&key));
        assert!(guard.lock_state().expect("state").rebuilding.is_empty());
        let doc = store.fuse_verdict("", "secret").expect("verdict");
        assert_eq!((doc.usage.steps, doc.evidence.len()), (0, 0));
    }

    #[test]
    fn an_observer_landing_after_the_sweep_cannot_bring_an_erased_run_back() {
        // A batch acknowledged just before an erasure began reaches the
        // observer after its fsync: here, after the erasure already swept
        // the guard, while it is still pending.
        let fixture = TempStore::open("late", FuseConfig::default());
        let store = fixture.store();
        let spans = vec![
            tool_span("late", "a", "search", "q", 1),
            tool_span("late", "b", "search", "q", 2),
        ];
        store.ingest_batch(spans.clone()).expect("ingests");
        let late: Vec<Arc<Span>> = spans.into_iter().map(Arc::new).collect();
        let guard = store.guard().expect("guard");
        *guard.after_forget.lock().expect("hook") = Some(Box::new(move |store: &Store| {
            store.observe_fuses(&late);
        }));
        store
            .erase(erasure::Subject::Session {
                session_id: "late".into(),
                tenant: String::new(),
            })
            .expect("erases");
        assert!(guard.after_forget.lock().expect("hook").is_none(), "it ran");
        let key = RunKey::new("", "late");
        assert!(!guard.lock_state().expect("state").runs.contains_key(&key));
    }

    #[test]
    fn the_observer_skips_spans_a_pending_erasure_covers() {
        let fixture = TempStore::open("masked", FuseConfig::default());
        let store = fixture.store();
        // An erasure that has begun — its mask is up — and not yet settled.
        store
            .erasures
            .begin(
                unix_now_ns(),
                erasure::Subject::Session {
                    session_id: "erased".into(),
                    tenant: String::new(),
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("begins");
        // A batch acknowledged before the erasure began, observed after it.
        let late: Vec<Arc<Span>> = [
            tool_span("erased", "a", "x", "1", 1),
            tool_span("kept", "a", "x", "1", 1),
        ]
        .into_iter()
        .map(Arc::new)
        .collect();
        store.observe_fuses(&late);
        let state = store.guard().expect("guard").lock_state().expect("state");
        assert!(!state.runs.contains_key(&RunKey::new("", "erased")));
        assert!(state.runs.contains_key(&RunKey::new("", "kept")));
    }

    #[test]
    fn a_tenant_erasure_rewrites_the_log_even_when_memory_holds_nothing_of_it() {
        let fixture = TempStore::open("tenantlog", FuseConfig::default());
        let store = fixture.store();
        let policy = store
            .put_fuse_policy(PolicySpec {
                name: "search".into(),
                tenant: "acme".into(),
                matches: PolicyMatch {
                    service: Some("acme-private-service".into()),
                },
                ..PolicySpec::default()
            })
            .expect("policy");
        store
            .delete_fuse_policy(Some("acme"), &policy.id)
            .expect("deletes");
        let path = fixture.dir.join(log::LOG_NAME);
        assert!(std::fs::read_to_string(&path)
            .expect("log")
            .contains("acme-private-service"));
        store
            .erase(erasure::Subject::Tenant {
                tenant: "acme".into(),
            })
            .expect("erases");
        let log = std::fs::read_to_string(&path).expect("log");
        assert!(!log.contains("acme"), "{log}");
    }

    #[test]
    fn live_and_rebuilt_state_take_the_policy_of_the_earliest_span() {
        let fixture = TempStore::open("firstspan", FuseConfig::default());
        let store = fixture.store();
        for (name, service, steps, action) in [
            ("root", "root-svc", 3, Action::Stop),
            ("child", "child-svc", 100, Action::Warn),
        ] {
            store
                .put_fuse_policy(PolicySpec {
                    name: name.into(),
                    matches: PolicyMatch {
                        service: Some(service.into()),
                    },
                    limits: Limits {
                        max_steps: Some(steps),
                        ..Limits::default()
                    },
                    action,
                    ..PolicySpec::default()
                })
                .expect("policy");
        }
        let in_service = |mut span: Span, service: &str| {
            span.service = service.into();
            span
        };
        // The children end, and arrive, first; the root agent span started
        // before them and arrives last.
        store
            .ingest_batch(vec![
                in_service(tool_span("nested", "c1", "x", "1", 10), "child-svc"),
                in_service(tool_span("nested", "c2", "x", "2", 20), "child-svc"),
            ])
            .expect("ingests");
        let mut root = in_service(tool_span("nested", "root", "plan", "", 0), "root-svc");
        root.attributes
            .insert("gen_ai.operation.name".into(), json!("invoke_agent"));
        root.end_time_ns = 50;
        let admission = store.ingest_batch(vec![root]).expect("ingests");
        assert_eq!(
            admission.signals.first().map(|signal| signal.verdict),
            Some(Verdict::Stop),
            "the root's stop policy, not the first arrival's: {:?}",
            admission.signals
        );
        let doc = store.fuse_verdict("", "nested").expect("verdict");
        assert_eq!((doc.policy.as_str(), doc.verdict), ("root", Verdict::Stop));
    }

    #[test]
    fn a_tenant_keeps_a_bounded_number_of_policies() {
        let fixture = TempStore::open("policycap", FuseConfig::default());
        let store = fixture.store();
        let spec = |index: usize| PolicySpec {
            name: format!("p{index}"),
            tenant: "acme".into(),
            matches: PolicyMatch {
                service: Some(format!("s{index}")),
            },
            ..PolicySpec::default()
        };
        for index in 0..MAX_POLICIES_PER_TENANT {
            store.put_fuse_policy(spec(index)).expect("policy");
        }
        match store.put_fuse_policy(spec(MAX_POLICIES_PER_TENANT)) {
            Err(Error::Conflict(message)) => assert!(message.contains("256"), "{message}"),
            other => panic!("expected the cap, got {other:?}"),
        }
        // Replacing one at the cap is not creating one, and another tenant
        // has its own allowance.
        store.put_fuse_policy(spec(0)).expect("replaces");
        store
            .put_fuse_policy(PolicySpec {
                tenant: "beta".into(),
                ..spec(MAX_POLICIES_PER_TENANT)
            })
            .expect("another tenant");
    }

    #[test]
    fn controls_are_bounded_per_tenant_and_a_kill_switch_is_never_dropped() {
        let mut state = State::default();
        let tenant = state.controls.entry("acme".into()).or_default();
        for index in 0..MAX_CONTROLLED_RUNS_PER_TENANT {
            tenant.insert(
                format!("r{index}"),
                Controls {
                    manual: (index % 2 == 0).then(|| (1, String::new())),
                    last_reset_ns: Some(index as u64),
                    grant: None,
                },
            );
        }
        // A known run needs no room; a new one takes the place of the
        // oldest run that is not under a kill switch.
        assert_eq!(
            state
                .room_for_control(&RunKey::new("acme", "r7"))
                .expect("room"),
            None
        );
        let dropped = state
            .room_for_control(&RunKey::new("acme", "new"))
            .expect("room");
        assert_eq!(dropped, Some(RunKey::new("acme", "r1")));
        assert_eq!(
            state.controls["acme"].len(),
            MAX_CONTROLLED_RUNS_PER_TENANT - 1
        );
        // Another tenant is unaffected.
        assert_eq!(
            state
                .room_for_control(&RunKey::new("beta", "x"))
                .expect("room"),
            None
        );

        // Every run under a kill switch: refused, nothing dropped.
        for controls in state.controls.get_mut("acme").expect("acme").values_mut() {
            controls.manual = Some((1, String::new()));
        }
        state
            .controls
            .get_mut("acme")
            .expect("acme")
            .insert("filler".into(), Controls::default());
        state
            .controls
            .get_mut("acme")
            .expect("acme")
            .get_mut("filler")
            .expect("filler")
            .manual = Some((1, String::new()));
        assert!(matches!(
            state.room_for_control(&RunKey::new("acme", "another")),
            Err(Error::Conflict(_))
        ));
        assert_eq!(state.controls["acme"].len(), MAX_CONTROLLED_RUNS_PER_TENANT);

        // A replay that brings back more is trimmed, kill switches kept.
        let runs = state.controls.get_mut("acme").expect("acme");
        for index in 0..5 {
            runs.insert(
                format!("spent{index}"),
                Controls {
                    last_reset_ns: Some(index),
                    ..Controls::default()
                },
            );
        }
        state.trim_controls();
        assert_eq!(state.controls["acme"].len(), MAX_CONTROLLED_RUNS_PER_TENANT);
        assert!(state.controls["acme"]
            .values()
            .all(|controls| controls.manual.is_some()));
    }

    #[test]
    fn a_log_mostly_superseded_is_compacted_at_open() {
        let mut fixture = TempStore::open("compactopen", FuseConfig::default());
        let path = fixture.dir.join(log::LOG_NAME);
        let mut lines = String::new();
        for at_ns in 0..2_000_u64 {
            let record = if at_ns % 2 == 0 {
                Record::Trip {
                    tenant: String::new(),
                    run: "flapping".into(),
                    reason: "again".into(),
                    at_ns,
                }
            } else {
                Record::Reset {
                    tenant: String::new(),
                    run: "flapping".into(),
                    grant: None,
                    at_ns,
                }
            };
            lines.push_str(&serde_json::to_string(&record).expect("encodes"));
            lines.push('\n');
        }
        std::fs::write(&path, lines).expect("writes");
        fixture.reopen(FuseConfig::default());
        let log = std::fs::read_to_string(&path).expect("log");
        assert_eq!(log.lines().count(), 1, "the last reset alone: {log}");
        let doc = fixture
            .store()
            .fuse_verdict("", "flapping")
            .expect("verdict");
        assert_eq!(
            doc.verdict,
            Verdict::Continue,
            "the last record was a reset"
        );
    }

    #[test]
    fn listings_follow_verdict_changes_per_tenant() {
        let fixture = TempStore::open(
            "listing",
            stop_on(Limits {
                max_steps: Some(5),
                ..Limits::default()
            }),
        );
        let store = fixture.store();
        let in_tenant = |tenant: &str, run: &str, count: usize| -> Vec<Span> {
            (0..count)
                .map(|index| {
                    let mut span = tool_span(run, &format!("s{index}"), "x", &index.to_string(), 1);
                    span.tenant = tenant.into();
                    span
                })
                .collect()
        };
        store
            .ingest_batch(in_tenant("acme", "quiet", 1))
            .expect("ingests");
        store
            .ingest_batch(in_tenant("acme", "warned", 4))
            .expect("ingests");
        store
            .ingest_batch(in_tenant("acme", "stopped", 5))
            .expect("ingests");
        store
            .ingest_batch(in_tenant("beta", "stopped", 5))
            .expect("ingests");
        assert_eq!(
            listed(store, Some("acme"), RunListFilter::Stop),
            ["stopped"]
        );
        assert_eq!(listed(store, Some("acme"), RunListFilter::Warn), ["warned"]);
        assert_eq!(
            listed(store, Some("acme"), RunListFilter::Active),
            ["stopped", "warned", "quiet"]
        );
        assert_eq!(
            listed(store, None, RunListFilter::Stop),
            ["stopped", "stopped"]
        );
        assert!(listed(store, Some("gamma"), RunListFilter::Active).is_empty());
        let limited = store
            .fuse_runs(None, RunListFilter::Active, 2)
            .expect("lists");
        assert_eq!(limited.len(), 2);

        // A raised tenant policy moves the warned run back to continue.
        store
            .put_fuse_policy(PolicySpec {
                name: DEFAULT_POLICY_NAME.into(),
                tenant: "acme".into(),
                limits: Limits {
                    max_steps: Some(50),
                    ..Limits::default()
                },
                action: Action::Stop,
                ..PolicySpec::default()
            })
            .expect("policy");
        assert!(listed(store, Some("acme"), RunListFilter::Warn).is_empty());
        assert_eq!(
            listed(store, Some("acme"), RunListFilter::Stop),
            ["stopped"]
        );
        // A kill switch moves a run into the stopped listing, a reset out.
        store.fuse_trip("acme", "quiet", "").expect("trips");
        assert_eq!(
            listed(store, Some("acme"), RunListFilter::Stop),
            ["quiet", "stopped"]
        );
        store.fuse_reset("acme", "quiet", None).expect("resets");
        assert_eq!(
            listed(store, Some("acme"), RunListFilter::Stop),
            ["stopped"]
        );
    }
}
