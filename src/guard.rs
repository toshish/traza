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
//! [`MAX_TRACKED_SPANS`] span contributions held across all of them.
//! Eviction loses nothing that matters — an evicted run rebuilds — except
//! the latch that a warn was already announced.

mod events;
pub mod http;
mod log;
mod policy;
mod state;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use events::{EventKind, EventPage, FuseEvent, MAX_WAIT_MS};
pub use policy::{
    Action, FuseConfig, FusePolicy, Limits, PolicyMatch, PolicySpec, DEFAULT_MAX_REPEATS,
    DEFAULT_MAX_RUNS, DEFAULT_POLICY_NAME, GLOBAL_POLICY_NAME,
};
pub use state::{NextStep, SpanRef, TopRepeat, TrippedBy, Usage};

use log::{FuseLog, Record};
use state::{Observation, RunState, Trip};

use crate::{erasure, semconv, Error, Result, Span, Store};

/// Ceiling on span contributions held across every live run — each a 32-byte
/// record under an 8-byte key, before hash-table overhead. Past it, whole
/// runs are evicted least-recently-active first; they rebuild from the store
/// when next asked about.
pub const MAX_TRACKED_SPANS: usize = 1_000_000;

/// Most spans a rebuild reads for one run. A larger run is rebuilt from its
/// earliest spans, and undercounts.
pub const REBUILD_MAX_SPANS: usize = 100_000;

/// A live run idle this long (server clock) is evicted.
const IDLE_NS: u64 = 6 * 60 * 60 * 1_000_000_000;

/// Longest manual-trip reason accepted, in bytes.
const MAX_REASON_BYTES: usize = 1024;

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

/// Which live runs a listing returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunListFilter {
    /// Every live run.
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

/// The guard's mutable state, behind one leaf lock that is never held while
/// the store is read or the log is written.
#[derive(Default)]
struct State {
    policies: Vec<FusePolicy>,
    controls: HashMap<RunKey, Controls>,
    runs: HashMap<RunKey, RunState>,
    /// Live runs by recency: tick → run. The oldest tick is evicted first.
    order: BTreeMap<u64, RunKey>,
    next_tick: u64,
    /// Marks held across every live run.
    tracked: usize,
    /// Runs being rebuilt: the observations that arrived while their stored
    /// spans were being read, and how many rebuilds are in flight.
    rebuilding: HashMap<RunKey, (usize, Vec<Observation>)>,
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
}

impl Guard {
    /// Opens the guard over `directory`, replaying `fuses.jsonl`; `None` when
    /// fuses are disabled, in which case the log is not even read — so a
    /// damaged log cannot keep a `--no-fuses` server from starting.
    pub(crate) fn open(directory: &Path, config: &FuseConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let (log, records) = FuseLog::open(directory)?;
        let mut state = State::default();
        for record in records {
            state.apply(&record);
        }
        // A kill switch outlives a restart, and so does its run's place in
        // the stopped list: give every manually stopped run a live entry,
        // to be rebuilt when asked about.
        let stopped: Vec<RunKey> = state
            .controls
            .iter()
            .filter(|(_, controls)| controls.manual.is_some())
            .map(|(key, _)| key.clone())
            .collect();
        let now = unix_now_ns();
        for key in stopped {
            state.entry(&key, now);
        }
        Ok(Some(Self {
            config: config.clone(),
            log: Mutex::new(log),
            state: Mutex::new(state),
            events: events::EventFeed::new(),
            trips: AtomicU64::new(0),
            warnings: AtomicU64::new(0),
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
        // Extraction — facts, step kinds, fingerprint digests — happens with
        // no lock held, grouped by run in first-appearance order.
        let mut index: HashMap<RunKey, usize> = HashMap::new();
        let mut groups: Vec<(RunKey, Vec<Observation>)> = Vec::new();
        for span in spans {
            let key = RunKey {
                tenant: span.tenant.clone(),
                run: semconv::run_id(&span.attributes, &span.trace_id),
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
                && !observations.iter().any(Observation::is_step)
            {
                // A run starts being followed at its first step. A trace of
                // HTTP and database spans is a "run" by identity too, but it
                // has nothing a fuse limits that a step would not bring, so
                // ordinary tracing traffic costs the observer this read and
                // no state. A later step starts following it from there,
                // and the first verdict asked for it rebuilds it whole from
                // the store, these spans included.
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
        state.evict(now, self.config.max_runs, MAX_TRACKED_SPANS);
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
        } else if judged.verdict == Verdict::Warn && !run.warned {
            run.warned = true;
            self.warnings.fetch_add(1, Ordering::Relaxed);
            self.announce(key, EventKind::Warn, &judged, now);
        } else if judged.verdict == Verdict::Continue {
            run.warned = false;
        }
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
    /// a tightened limit trips now rather than at the run's next batch.
    fn settle_tenant(&self, state: &mut State, tenant: &str, now: u64) {
        let keys: Vec<RunKey> = state
            .runs
            .keys()
            .filter(|key| key.tenant == tenant)
            .cloned()
            .collect();
        for key in keys {
            self.settle(state, &key, now);
        }
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
    /// A policy stop this judgement found that the run does not carry yet.
    new_trip: Option<Trip>,
}

impl State {
    /// Applies one durable record, in log order.
    fn apply(&mut self, record: &Record) {
        match record {
            Record::Policy { policy } => {
                self.policies.retain(|existing| {
                    existing.id != policy.id
                        && !(existing.tenant == policy.tenant && existing.name == policy.name)
                });
                self.policies.push(policy.clone());
            }
            Record::DeletePolicy { id, .. } => self.policies.retain(|policy| &policy.id != id),
            Record::Trip {
                tenant,
                run,
                reason,
                at_ns,
            } => {
                self.controls
                    .entry(RunKey::new(tenant, run))
                    .or_default()
                    .manual = Some((*at_ns, reason.clone()));
            }
            Record::Reset {
                tenant,
                run,
                grant,
                at_ns,
            } => {
                let controls = self.controls.entry(RunKey::new(tenant, run)).or_default();
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

    /// The durable state as the records that reproduce it, for a rewrite:
    /// every policy, then per run its last reset (carrying the merged grant)
    /// before its kill switch — a kill switch is only ever active AFTER the
    /// last reset, so that order replays to the same controls.
    fn records(&self) -> Vec<Record> {
        let mut policies = self.policies.clone();
        policies.sort_by(|left, right| {
            (left.created_at_ns, &left.tenant, &left.name).cmp(&(
                right.created_at_ns,
                &right.tenant,
                &right.name,
            ))
        });
        let mut records: Vec<Record> = policies
            .into_iter()
            .map(|policy| Record::Policy { policy })
            .collect();
        let mut controls: Vec<(&RunKey, &Controls)> = self.controls.iter().collect();
        controls.sort_by(|left, right| left.0.cmp(right.0));
        for (key, controls) in controls {
            if let Some(at_ns) = controls.last_reset_ns {
                records.push(Record::Reset {
                    tenant: key.tenant.clone(),
                    run: key.run.clone(),
                    grant: controls.grant,
                    at_ns,
                });
            }
            if let Some((at_ns, reason)) = &controls.manual {
                records.push(Record::Trip {
                    tenant: key.tenant.clone(),
                    run: key.run.clone(),
                    reason: reason.clone(),
                    at_ns: *at_ns,
                });
            }
        }
        records
    }

    /// Drops one live run and its share of the budget.
    fn remove(&mut self, key: &RunKey) {
        if let Some(run) = self.runs.remove(key) {
            self.order.remove(&run.tick);
            self.tracked = self.tracked.saturating_sub(run.tracked());
        }
    }

    /// The live entry for `key`, created empty (and incomplete) when absent,
    /// and moved to the recent end of the eviction order.
    fn entry(&mut self, key: &RunKey, now: u64) {
        let tick = self.next_tick;
        self.next_tick += 1;
        let run = self
            .runs
            .entry(key.clone())
            .or_insert_with(|| RunState::new(false));
        let previous = std::mem::replace(&mut run.tick, tick);
        run.last_seen_ns = now;
        self.order.remove(&previous);
        self.order.insert(tick, key.clone());
    }

    /// Installs a rebuilt run in place of whatever was live for it.
    fn install(&mut self, key: &RunKey, mut run: RunState, now: u64) {
        let tick = self.next_tick;
        self.next_tick += 1;
        run.tick = tick;
        run.last_seen_ns = now;
        self.tracked += run.tracked();
        if let Some(previous) = self.runs.insert(key.clone(), run) {
            self.tracked = self.tracked.saturating_sub(previous.tracked());
            self.order.remove(&previous.tick);
            if let Some(run) = self.runs.get_mut(key) {
                // What was announced live stands: a warn already sent stays
                // sent, and a stop observed live stays sticky with the time
                // and reasons it tripped with — a rebuild refines usage, it
                // does not un-blow a fuse.
                run.warned |= previous.warned;
                if previous.trip.is_some() {
                    run.trip = previous.trip;
                }
            }
        }
        self.order.insert(tick, key.clone());
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
        let run = self.runs.get(key).unwrap_or(&empty);
        let controls = self.controls.get(key);
        let resolved = policy::resolve(
            &self.policies,
            config,
            &key.tenant,
            run.service.as_deref().unwrap_or(""),
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
        if let Some(trip) = &run.trip {
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
            judged.reasons = assessment.reached;
            judged.reasons.extend(assessment.approaching);
            judged.evidence = assessment.evidence;
        }
        judged
    }

    /// Evicts idle runs, then least-recently-active ones until both bounds
    /// hold. A run under an active kill switch is never evicted: it must stay
    /// listable as stopped, and such runs are few.
    fn evict(&mut self, now: u64, max_runs: usize, max_tracked: usize) {
        let idle_before = now.saturating_sub(IDLE_NS);
        let mut victims = Vec::new();
        let mut runs = self.runs.len();
        let mut tracked = self.tracked;
        for key in self.order.values() {
            let Some(run) = self.runs.get(key) else {
                continue;
            };
            let over = runs > max_runs.max(1) || tracked > max_tracked;
            if !over && run.last_seen_ns >= idle_before {
                break;
            }
            let pinned = self
                .controls
                .get(key)
                .is_some_and(|controls| controls.manual.is_some());
            if pinned {
                continue;
            }
            runs -= 1;
            tracked = tracked.saturating_sub(run.tracked());
            victims.push(key.clone());
        }
        for key in victims {
            if let Some(run) = self.runs.remove(&key) {
                self.order.remove(&run.tick);
                self.tracked = self.tracked.saturating_sub(run.tracked());
            }
        }
    }

    fn doc(&self, key: &RunKey, judged: Judgement, source: Source) -> VerdictDoc {
        let usage = self.runs.get(key).map(RunState::usage).unwrap_or_default();
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
}

/// Refuses a run id no span could carry.
fn validate_run(run: &str) -> Result<()> {
    if run.is_empty() {
        return Err(Error::Invalid("run id is empty".to_owned()));
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
    /// carries
    /// is a fresh run with zero usage — `continue`, since a fresh run is
    /// allowed to start. If a rebuild fails (an exhausted deadline), a run
    /// with live state is answered from it, marked `live`, and the rebuild is
    /// retried on the next query.
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
        let now = unix_now_ns();
        {
            let mut state = guard.lock_state()?;
            if state.runs.get(&key).is_some_and(|live| live.complete) {
                state.entry(&key, now);
                guard.settle(&mut state, &key, now);
                let judged = state.judge(&guard.config, &key, next, now);
                return Ok(state.doc(&key, judged, Source::Live));
            }
            state.rebuilding.entry(key.clone()).or_default().0 += 1;
        }

        // The store is read with no guard lock held. Observations that land
        // meanwhile are captured for this run and replayed over the result,
        // so nothing admitted during the read is lost or counted twice.
        let deadline = self.query_deadline(Instant::now());
        let read = self.run_spans(Some(tenant), run, REBUILD_MAX_SPANS, deadline);
        let observations: Option<Vec<Observation>> = read.as_ref().ok().map(|stored| {
            stored
                .spans
                .iter()
                .map(|span| Observation::of(self, span))
                .collect()
        });

        let mut state = guard.lock_state()?;
        let pending = match state.rebuilding.get_mut(&key) {
            Some((inflight, pending)) => {
                *inflight -= 1;
                let captured = pending.clone();
                if *inflight == 0 {
                    state.rebuilding.remove(&key);
                }
                captured
            }
            None => Vec::new(),
        };
        let observations = match (observations, read) {
            (Some(observations), _) => observations,
            (None, Err(error)) => {
                if state.runs.contains_key(&key) {
                    state.entry(&key, now);
                    let judged = state.judge(&guard.config, &key, next, now);
                    return Ok(state.doc(&key, judged, Source::Live));
                }
                return Err(error);
            }
            (None, Ok(_)) => unreachable!("observations exist exactly when the read succeeded"),
        };
        // A concurrent rebuild may have installed first; its answer stands.
        if !state.runs.get(&key).is_some_and(|live| live.complete) {
            let reset_at = state
                .controls
                .get(&key)
                .and_then(|controls| controls.last_reset_ns);
            let policies = state.policies.clone();
            let grant = state.controls.get(&key).and_then(|controls| controls.grant);
            let resolve = |service: &str| {
                let resolved = policy::resolve(&policies, &guard.config, tenant, service);
                let limits = match &grant {
                    Some(grant) => resolved.limits.overlaid(grant),
                    None => resolved.limits,
                };
                (limits, resolved.action)
            };
            let mut rebuilt = state::replay(&observations, reset_at, &resolve);
            for observation in &pending {
                rebuilt.absorb(observation, false);
            }
            state.install(&key, rebuilt, now);
            guard.settle(&mut state, &key, now);
            state.evict(now, guard.config.max_runs, MAX_TRACKED_SPANS);
        } else {
            state.entry(&key, now);
        }
        let judged = state.judge(&guard.config, &key, next, now);
        Ok(state.doc(&key, judged, Source::Rebuilt))
    }

    /// Live runs, most recently active first, narrowed to `tenant` when
    /// given and to `filter`, at most `limit` of them.
    ///
    /// Answered from memory alone — a listing never rebuilds, so a run this
    /// process has followed only partly lists with the usage it has seen.
    /// Ask for the run itself ([`Self::fuse_verdict`]) for the whole of it.
    pub fn fuse_runs(
        &self,
        tenant: Option<&str>,
        filter: RunListFilter,
        limit: usize,
    ) -> Result<Vec<VerdictDoc>> {
        let guard = self.guard()?;
        let now = unix_now_ns();
        let state = guard.lock_state()?;
        let mut docs = Vec::new();
        for key in state.order.values().rev() {
            if docs.len() >= limit {
                break;
            }
            if tenant.is_some_and(|tenant| tenant != key.tenant) {
                continue;
            }
            let judged = state.judge(&guard.config, key, None, now);
            let keep = match filter {
                RunListFilter::Active => true,
                RunListFilter::Warn => judged.verdict == Verdict::Warn,
                RunListFilter::Stop => judged.verdict == Verdict::Stop,
            };
            if keep {
                docs.push(state.doc(key, judged, Source::Live));
            }
        }
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
            let log = guard.lock_log()?;
            log.append(&record)?;
            let mut state = guard.lock_state()?;
            state.apply(&record);
            state.entry(&key, now);
            guard.trips.fetch_add(1, Ordering::Relaxed);
            let judged = state.judge(&guard.config, &key, None, now);
            guard.announce(&key, EventKind::ManualStop, &judged, now);
            drop(log);
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
            let log = guard.lock_log()?;
            log.append(&record)?;
            let mut state = guard.lock_state()?;
            state.apply(&record);
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
            drop(log);
        }
        self.fuse_verdict(tenant, run)
    }

    /// Creates the policy `spec` names, or replaces the tenant's policy of
    /// that name (keeping its id and creation time). Live runs of the tenant
    /// are re-judged at once.
    pub fn put_fuse_policy(&self, spec: PolicySpec) -> Result<FusePolicy> {
        let guard = self.guard()?;
        spec.validate()?;
        let now = unix_now_ns();
        let log = guard.lock_log()?;
        let existing = guard
            .lock_state()?
            .policies
            .iter()
            .find(|policy| policy.tenant == spec.tenant && policy.name == spec.name)
            .map(|policy| (policy.id.clone(), policy.created_at_ns));
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
        let mut state = guard.lock_state()?;
        state.apply(&record);
        guard.settle_tenant(&mut state, &policy.tenant, now);
        drop(log);
        Ok(policy)
    }

    /// Stored policies, by tenant then name; narrowed to `tenant` when given.
    pub fn fuse_policies(&self, tenant: Option<&str>) -> Result<Vec<FusePolicy>> {
        let guard = self.guard()?;
        let state = guard.lock_state()?;
        let mut policies: Vec<FusePolicy> = state
            .policies
            .iter()
            .filter(|policy| tenant.map_or(true, |tenant| policy.tenant == tenant))
            .cloned()
            .collect();
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
        let log = guard.lock_log()?;
        let found = guard
            .lock_state()?
            .policies
            .iter()
            .find(|policy| policy.id == id && tenant.map_or(true, |tenant| policy.tenant == tenant))
            .cloned();
        let Some(policy) = found else {
            return Ok(None);
        };
        let record = Record::DeletePolicy {
            id: policy.id.clone(),
            at_ns: now,
        };
        log.append(&record)?;
        let mut state = guard.lock_state()?;
        state.apply(&record);
        guard.settle_tenant(&mut state, &policy.tenant, now);
        drop(log);
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
    /// fingerprints that carry tool and model names, the event feed holds run
    /// ids and reasons, and `fuses.jsonl` holds run ids — which are session
    /// ids — with kill-switch reasons. So: every live run any purged span
    /// contributed to is dropped (asked about again, it is rebuilt from what
    /// the store still holds); a tenant subject drops the tenant's runs,
    /// events, policies and controls wholesale; a session or trace subject
    /// drops that run's events and controls. When the durable state changed,
    /// `fuses.jsonl` is rewritten without it. Idempotent, so a resumed
    /// erasure may run it again.
    pub(crate) fn forget_fuse_subject(
        &self,
        subject: &erasure::Subject,
        keys: &std::collections::HashSet<(String, String, String)>,
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
        let mut purged: HashMap<&str, std::collections::HashSet<u64>> = HashMap::new();
        for (key_tenant, trace_id, span_id) in keys {
            purged
                .entry(key_tenant.as_str())
                .or_default()
                .insert(state::span_key(trace_id, span_id));
        }

        let log = guard.lock_log()?;
        let records = {
            let mut state = guard.lock_state()?;
            let doomed: Vec<RunKey> = state
                .runs
                .iter()
                .filter(|(key, run)| {
                    names(&key.tenant, &key.run)
                        || purged
                            .get(key.tenant.as_str())
                            .is_some_and(|spans| run.holds_any(spans))
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in &doomed {
                state.remove(key);
            }
            state
                .rebuilding
                .retain(|key, _| !names(&key.tenant, &key.run));
            guard
                .events
                .retain(&|event: &FuseEvent| !names(&event.tenant, &event.run));
            let before = (state.policies.len(), state.controls.len());
            if whole_tenant {
                state
                    .policies
                    .retain(|policy| Some(policy.tenant.as_str()) != tenant);
            }
            state
                .controls
                .retain(|key, _| !names(&key.tenant, &key.run));
            ((state.policies.len(), state.controls.len()) != before).then(|| state.records())
        };
        // Written with the state lock released — ingest observers never wait
        // on this fsync — but under the log lock, which every durable
        // mutation takes first, so nothing can interleave with the rewrite.
        if let Some(records) = records {
            log.rewrite(&records)?;
        }
        drop(log);
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

    #[test]
    fn eviction_keeps_the_bound_and_never_drops_a_kill_switch() {
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
        let live: Vec<String> = store
            .fuse_runs(None, RunListFilter::Active, 10)
            .expect("lists")
            .into_iter()
            .map(|doc| doc.run)
            .collect();
        assert_eq!(live, ["c", "pinned"], "newest first; the kill switch stays");
        // An evicted run is not lost: it rebuilds.
        let doc = store.fuse_verdict("", "a").expect("verdict");
        assert_eq!((doc.source, doc.usage.steps), (Source::Rebuilt, 1));
    }
}
