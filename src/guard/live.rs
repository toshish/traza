//! The live view: runs with recent activity, most recently changed first,
//! and the change feed a stream of them follows.
//!
//! Built from the guard's live state alone — never a store fold. Every live
//! run carries a position in a change order, re-stamped whenever something
//! a viewer would see changes: a span arrives, a pre-call check asks about
//! its next step, its verdict class moves, a nudge is queued or delivered.
//! An index by that position, across every tenant and per tenant, answers
//! "newest first" by walking from its end, and "what changed since position
//! p" by walking from p. A listing walks the runs changed within the window,
//! newest first, until it has `limit` rows: it skips runs that changed
//! recently without activity (a verdict that moved, a nudge), so it reads at
//! most every run the guard holds, never the store.
//!
//! The re-stamp is what the view costs ingest: per run a batch touches, one
//! removal and one insertion in each of the two indexes and an atomic
//! store, under the guard's state lock the fold already holds — plus, per
//! step span, the step's agent and label, read beside its fingerprint.
//!
//! **Activity** is narrower than change: a span arriving, or a pre-call
//! check (one naming its `next` step). A run is live while its last
//! activity is within the window the caller names
//! (`--live-window-seconds`). A verdict read — a check naming no next step
//! included — is neither, so a dashboard polling a run cannot keep it live.
//!
//! What the view shows is what this process has seen: a run followed from
//! some point after a restart lists with the usage seen since, like
//! `GET /v1/fuses/runs`. Parked runs — stopped and evicted — are not live.
//! A run with no span yet holds no state, so it is not listed until its
//! first span arrives, however many checks it makes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::state::{LiveStep, NextStep, RunState};
use super::{unix_now_ns, validate_run, validate_tenant, Guard, RunKey, State, Verdict};
use crate::semconv::StepKind;
use crate::{Result, Store};

/// How often a waiting stream looks for changes. A change reaches a
/// stream within this, at the cost of one atomic read per look; waiting
/// streams take no lock until something changed.
const POLL: Duration = Duration::from_millis(200);

/// Most runs the feed remembers leaving the live set (evicted from the
/// guard's memory), newest kept, so a stream can tell its client to drop
/// them. A stream that fell further behind than this starts over from a
/// listing.
const MAX_LIVE_GONE: usize = 1024;

/// Live runs by change position: across every tenant, and per tenant —
/// and the runs that left the live set, by the change position of their
/// leaving.
#[derive(Default)]
pub(super) struct LiveIndex {
    all: BTreeMap<u64, Arc<RunKey>>,
    by_tenant: HashMap<String, BTreeMap<u64, Arc<RunKey>>>,
    /// Runs that left, at most [`MAX_LIVE_GONE`], newest kept.
    gone: BTreeMap<u64, Arc<RunKey>>,
    /// Leavings at or below this position may have been forgotten.
    gone_floor: u64,
}

impl LiveIndex {
    pub(super) fn insert(&mut self, key: &Arc<RunKey>, seq: u64) {
        self.all.insert(seq, Arc::clone(key));
        match self.by_tenant.get_mut(&key.tenant) {
            Some(map) => {
                map.insert(seq, Arc::clone(key));
            }
            None => {
                let mut map = BTreeMap::new();
                map.insert(seq, Arc::clone(key));
                self.by_tenant.insert(key.tenant.clone(), map);
            }
        }
    }

    pub(super) fn remove(&mut self, key: &RunKey, seq: u64) {
        self.all.remove(&seq);
        if let Some(map) = self.by_tenant.get_mut(&key.tenant) {
            map.remove(&seq);
            if map.is_empty() {
                self.by_tenant.remove(&key.tenant);
            }
        }
    }

    fn scope(&self, tenant: Option<&str>) -> Option<&BTreeMap<u64, Arc<RunKey>>> {
        match tenant {
            Some(tenant) => self.by_tenant.get(tenant),
            None => Some(&self.all),
        }
    }

    /// Records that `key` left the live set at change position `seq`.
    fn leave(&mut self, key: Arc<RunKey>, seq: u64) {
        self.gone.insert(seq, key);
        while self.gone.len() > MAX_LIVE_GONE {
            if let Some((oldest, _)) = self.gone.pop_first() {
                self.gone_floor = self.gone_floor.max(oldest);
            }
        }
    }

    /// Forgets every leaving up to `seq`: an erasure's, which restarts
    /// every stream from a listing anyway, and must not keep the run ids
    /// it removed.
    pub(super) fn forget_gone(&mut self, seq: u64) {
        self.gone.clear();
        self.gone_floor = self.gone_floor.max(seq);
    }
}

/// Why a run left a stream's view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveRemovalReason {
    /// The guard let go of it to stay inside its memory bounds
    /// (`--fuse-max-runs`, its span budget, or six idle hours); a stopped
    /// run is parked.
    Evicted,
    /// Its last activity fell out of the live window.
    Expired,
}

/// A run a stream's client must drop: it left the live view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveRemoval {
    /// The run id.
    pub run: String,
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// Why it left.
    pub reason: LiveRemovalReason,
}

impl LiveRemoval {
    fn of(key: &RunKey, reason: LiveRemovalReason) -> Self {
        Self {
            run: key.run.clone(),
            tenant: key.tenant.clone(),
            reason,
        }
    }
}

/// Where the live view's change feed stands, shared lock-free between the
/// guard's state (which advances it) and waiting streams (which read it).
#[derive(Default)]
pub(super) struct Feed {
    /// The newest change position handed out.
    pub(super) seq: AtomicU64,
    /// Bumped when runs leave the view in bulk — an erasure — so a stream
    /// starts over from a fresh listing instead of trusting its position.
    pub(super) epoch: AtomicU64,
}

/// One live run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveRun {
    /// The run id.
    pub run: String,
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The agent the run is: its first step's agent, else its first span's
    /// service. Telemetry text.
    pub agent: Option<String>,
    /// The service of the run's first span, which picks its policy.
    pub service: Option<String>,
    /// Server clock at the run's last span arrival or pre-call check.
    pub last_activity_ns: u64,
    /// The run's first span's start, by the spans' clocks.
    pub started_ns: Option<u64>,
    /// Steps recorded: LLM, tool and agent spans.
    pub steps: u64,
    /// The step the run is on: the one a pre-call check asked about when no
    /// span has arrived since (`pending: true`), else the latest-starting
    /// step recorded.
    pub current: Option<LiveStep>,
    /// Priced cost, USD.
    pub cost_usd: f64,
    /// Tokens.
    pub tokens: u64,
    /// Spans with status `error`.
    pub errors: u64,
    /// The guard's verdict.
    pub verdict: Verdict,
    /// Why, when the verdict is not `continue`.
    pub reasons: Vec<String>,
    /// The policy the run is held to.
    pub policy: String,
    /// Nudges queued for the run and not yet delivered or expired.
    pub pending_nudges: usize,
    /// How earlier runs that took this run's path ended, from the run's
    /// behaviour path in memory and its tenant's cached Fate Map; absent when
    /// no whole path is held for the run (see
    /// [`Store::fill_live_foresight`](crate::Store)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foresight: Option<crate::fate::LiveForesight>,
    /// Whether the row lacks `foresight` only because the one-time read of
    /// its run that would give it foresight was not made by this request:
    /// past the [`LIVE_FORESIGHT_REBUILDS`](crate::fate::LIVE_FORESIGHT_REBUILDS)
    /// reads one request makes, past the request's query budget, or held
    /// back after a recent read of the run failed. A later request makes
    /// it. Absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub foresight_deferred: bool,
}

/// A listing of live runs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LivePage {
    /// The runs, most recently changed first.
    pub runs: Vec<LiveRun>,
    /// The activity window the listing applied, in nanoseconds.
    pub window_ns: u64,
    /// Whether more live runs matched than the limit returned.
    pub truncated: bool,
}

/// A stream's position in the change feed, and the runs its client holds:
/// every row it was sent and not told to drop, with the last activity the
/// row showed — so the stream can tell the client when one leaves the
/// view. Opaque: start a stream with [`LiveCursor::default`], which reads
/// as "nothing seen yet". Holds at most the `held` bound the stream is
/// served with (about 64 bytes a run; the run ids are the guard's own).
#[derive(Clone, Debug, Default)]
pub struct LiveCursor {
    epoch: u64,
    seq: u64,
    started: bool,
    held: HashMap<Arc<RunKey>, u64>,
    /// The same runs by last activity, earliest first: the next to expire.
    by_activity: BTreeSet<(u64, Arc<RunKey>)>,
}

impl LiveCursor {
    /// Notes that the client now holds `key`'s row, last active at
    /// `activity`.
    fn hold(&mut self, key: &Arc<RunKey>, activity: u64) {
        if let Some(previous) = self.held.insert(Arc::clone(key), activity) {
            self.by_activity.remove(&(previous, Arc::clone(key)));
        }
        self.by_activity.insert((activity, Arc::clone(key)));
    }

    /// Notes that the client drops `key`'s row; whether it held one.
    fn drop_row(&mut self, key: &RunKey) -> bool {
        let Some((shared, activity)) = self.held.remove_entry(key) else {
            return false;
        };
        self.by_activity.remove(&(activity, shared));
        true
    }

    /// Starts over from a listing whose rows are `rows`.
    fn reset(&mut self, rows: Vec<(Arc<RunKey>, u64)>) {
        self.held.clear();
        self.by_activity.clear();
        for (key, activity) in rows {
            self.hold(&key, activity);
        }
    }

    /// When the held row that expires first does, server clock.
    fn next_expiry(&self, window_ns: u64) -> Option<u64> {
        self.by_activity
            .first()
            .map(|(activity, _)| activity.saturating_add(window_ns))
    }
}

/// What a stream should send next.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveUpdate {
    /// Start over from this listing: the stream's first read, after an
    /// erasure, or when more runs changed at once than one update carries —
    /// or the client would hold more than the stream keeps track of.
    Snapshot(LivePage),
    /// Runs that left the view, then runs that changed, oldest change
    /// first. Both possibly empty: changes happened, none of them to a live
    /// run this stream may see.
    Changed {
        /// Rows the client holds and must drop: runs the guard evicted, or
        /// whose last activity fell out of the window.
        removed: Vec<LiveRemoval>,
        /// Rows to add or replace.
        runs: Vec<LiveRun>,
    },
    /// Nothing changed within the wait.
    Idle,
}

impl State {
    /// The next change position, and its time — never earlier than the
    /// last one's, so positions and times sort together and a listing can
    /// stop at the first change older than its window.
    fn change_stamp(&mut self, now: u64) -> (u64, u64) {
        self.next_change += 1;
        self.last_change_ns = self.last_change_ns.max(now);
        self.live_feed
            .seq
            .store(self.next_change, Ordering::Release);
        (self.next_change, self.last_change_ns)
    }

    /// Stamps a run joining the live set.
    pub(super) fn live_admit(&mut self, key: &Arc<RunKey>, run: &mut RunState, now: u64) {
        let (seq, at) = self.change_stamp(now);
        run.changed = seq;
        run.changed_ns = at;
        self.live.insert(key, seq);
    }

    /// Re-stamps a live run that changed; `activity` when a span arrived or
    /// a pre-call check asked, which is what keeps it in the window.
    pub(super) fn live_touch(&mut self, key: &RunKey, now: u64, activity: bool) {
        let Some((shared, run)) = self.runs.get_key_value(key) else {
            return;
        };
        let (shared, previous) = (Arc::clone(shared), run.changed);
        let (seq, at) = self.change_stamp(now);
        self.live.remove(key, previous);
        self.live.insert(&shared, seq);
        if let Some(run) = self.runs.get_mut(key) {
            run.changed = seq;
            run.changed_ns = at;
            if activity {
                run.last_activity_ns = run.last_activity_ns.max(now);
            }
        }
    }

    /// Records that `key`, just taken out of the live set for good (an
    /// eviction), left the view — a change, so waiting streams wake and
    /// tell their clients to drop it.
    pub(super) fn live_leave(&mut self, key: Arc<RunKey>, now: u64) {
        let (seq, _) = self.change_stamp(now);
        self.live.leave(key, seq);
    }

    /// One live run as the view shows it, judged now.
    fn live_row(&self, guard: &Guard, key: &RunKey, run: &RunState, now: u64) -> LiveRun {
        let judged = self.judge(&guard.config, key, None, now);
        let usage = run.usage();
        let (agent, started_ns, latest) = run.live_labels();
        let current = match &run.pending {
            Some(pending) if pending.started_ns > run.last_arrival_ns => Some(pending.clone()),
            _ => latest,
        };
        LiveRun {
            run: key.run.clone(),
            tenant: key.tenant.clone(),
            agent,
            service: run.service.clone(),
            last_activity_ns: run.last_activity_ns,
            started_ns,
            steps: usage.steps,
            current,
            cost_usd: usage.cost_usd,
            tokens: usage.tokens,
            errors: usage.errors,
            verdict: judged.verdict,
            reasons: judged.reasons,
            policy: judged.policy,
            pending_nudges: 0,
            foresight: None,
            foresight_deferred: false,
        }
    }

    /// Live runs of `tenant` (every tenant: `None`) active since `cutoff`,
    /// most recently changed first, at most `limit` — each row's run and
    /// last activity also pushed to `held`, when given.
    #[allow(clippy::too_many_arguments)]
    fn live_page(
        &self,
        guard: &Guard,
        tenant: Option<&str>,
        limit: usize,
        cutoff: u64,
        window_ns: u64,
        now: u64,
        mut held: Option<&mut Vec<(Arc<RunKey>, u64)>>,
    ) -> LivePage {
        let mut runs = Vec::new();
        let mut truncated = false;
        if let Some(index) = self.live.scope(tenant) {
            for key in index.values().rev() {
                let Some(run) = self.runs.get(&**key) else {
                    continue;
                };
                // Changes are stamped in time order: nothing older than this
                // one can be in the window.
                if run.changed_ns < cutoff {
                    break;
                }
                if run.last_activity_ns < cutoff {
                    continue;
                }
                if runs.len() == limit {
                    truncated = true;
                    break;
                }
                runs.push(self.live_row(guard, key, run, now));
                if let Some(held) = held.as_deref_mut() {
                    held.push((Arc::clone(key), run.last_activity_ns));
                }
            }
        }
        LivePage {
            runs,
            window_ns,
            truncated,
        }
    }
}

impl Store {
    /// Runs with activity — a span arriving, or a pre-call check — in the
    /// last `window`, most recently changed first, narrowed to `tenant`
    /// when given, at most `limit` of them; see the module docs for what
    /// the view does and does not show. `pending_nudges` is left at zero for
    /// the caller to fill.
    pub(crate) fn guard_live_page(
        &self,
        tenant: Option<&str>,
        limit: usize,
        window: Duration,
    ) -> Result<LivePage> {
        let guard = self.guard()?;
        let now = unix_now_ns();
        let window_ns = window.as_nanos().min(u128::from(u64::MAX)) as u64;
        let cutoff = now.saturating_sub(window_ns);
        let state = guard.lock_state()?;
        Ok(state.live_page(guard, tenant, limit, cutoff, window_ns, now, None))
    }

    /// Waits up to `wait` for live runs to change after `cursor` — or for a
    /// row its client holds to fall out of the window — and says what a
    /// stream should send; advances `cursor`. The runs its client holds that
    /// left the view since travel first, as removals: evicted by the guard,
    /// or expired (no activity within the window). At most `limit` runs
    /// travel as changes, and the client holds at most `held` rows — more of
    /// either, and the stream starts over from a listing, which is cheaper
    /// for both ends than a flood; so does a stream that fell further behind
    /// the feed's memory of evictions than it reaches.
    pub(crate) fn guard_live_wait(
        &self,
        tenant: Option<&str>,
        cursor: &mut LiveCursor,
        limit: usize,
        held: usize,
        window: Duration,
        wait: Duration,
    ) -> Result<LiveUpdate> {
        let guard = self.guard()?;
        let feed = &guard.live_feed;
        let window_ns = window.as_nanos().min(u128::from(u64::MAX)) as u64;
        let snapshot = |cursor: &mut LiveCursor| {
            self.guard_live_snapshot(guard, tenant, cursor, limit, window)
                .map(LiveUpdate::Snapshot)
        };
        if !cursor.started || cursor.epoch != feed.epoch.load(Ordering::Acquire) {
            return snapshot(cursor);
        }
        let until = Instant::now() + wait;
        loop {
            if feed.epoch.load(Ordering::Acquire) != cursor.epoch {
                return snapshot(cursor);
            }
            if feed.seq.load(Ordering::Acquire) > cursor.seq {
                break;
            }
            let now = unix_now_ns();
            let expiry = cursor.next_expiry(window_ns);
            if expiry.is_some_and(|at| at <= now) {
                break;
            }
            let mut left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(LiveUpdate::Idle);
            }
            if let Some(at) = expiry {
                left = left.min(Duration::from_nanos(at - now));
            }
            std::thread::sleep(left.min(POLL));
        }
        let now = unix_now_ns();
        let cutoff = now.saturating_sub(window_ns);
        let state = guard.lock_state()?;
        if cursor.seq < state.live.gone_floor {
            drop(state);
            return snapshot(cursor);
        }
        let in_scope = |key: &RunKey| tenant.map_or(true, |tenant| key.tenant == tenant);
        let mut removed = Vec::new();
        // Leavings first: a run evicted and then back again leaves and
        // returns, in that order.
        for (_, key) in state.live.gone.range(cursor.seq.saturating_add(1)..) {
            if in_scope(key) && cursor.drop_row(key) {
                removed.push(LiveRemoval::of(key, LiveRemovalReason::Evicted));
            }
        }
        let mut runs = Vec::new();
        if let Some(index) = state.live.scope(tenant) {
            for (_, key) in index.range(cursor.seq.saturating_add(1)..) {
                let Some(run) = state.runs.get(&**key) else {
                    continue;
                };
                if run.last_activity_ns < cutoff {
                    // It changed without activity — a verdict, a nudge —
                    // after its activity left the window.
                    if cursor.drop_row(key) {
                        removed.push(LiveRemoval::of(key, LiveRemovalReason::Expired));
                    }
                    continue;
                }
                if runs.len() == limit {
                    drop(state);
                    return snapshot(cursor);
                }
                runs.push(state.live_row(guard, key, run, now));
                cursor.hold(key, run.last_activity_ns);
                if cursor.held.len() > held {
                    drop(state);
                    return snapshot(cursor);
                }
            }
        }
        // Rows whose last activity fell out of the window with no change
        // since: every activity re-stamps a run, so the held activity is
        // the run's own.
        while let Some((activity, key)) = cursor.by_activity.first().cloned() {
            if activity >= cutoff {
                break;
            }
            cursor.drop_row(&key);
            removed.push(LiveRemoval::of(&key, LiveRemovalReason::Expired));
        }
        cursor.seq = state.next_change;
        Ok(LiveUpdate::Changed { removed, runs })
    }

    /// A listing that restarts a stream at the feed's current position: its
    /// rows are all the client holds from here.
    fn guard_live_snapshot(
        &self,
        guard: &Guard,
        tenant: Option<&str>,
        cursor: &mut LiveCursor,
        limit: usize,
        window: Duration,
    ) -> Result<LivePage> {
        let now = unix_now_ns();
        let window_ns = window.as_nanos().min(u128::from(u64::MAX)) as u64;
        let state = guard.lock_state()?;
        // Read under the state lock with the listing, so no change lands
        // between the two unseen.
        cursor.epoch = guard.live_feed.epoch.load(Ordering::Acquire);
        cursor.seq = state.next_change;
        cursor.started = true;
        let mut held = Vec::new();
        let page = state.live_page(
            guard,
            tenant,
            limit,
            now.saturating_sub(window_ns),
            window_ns,
            now,
            Some(&mut held),
        );
        drop(state);
        cursor.reset(held);
        Ok(page)
    }

    /// Records that `run` asked the pre-call gate about its `next` step: it
    /// is active, and that step is what it is on until a span arrives.
    /// Nothing for a run the guard holds no live state for (a run with no
    /// span yet costs no memory, however often it asks).
    pub(crate) fn guard_note_check(&self, tenant: &str, run: &str, next: &NextStep) {
        let Some(guard) = self.guard.as_ref() else {
            return;
        };
        if validate_tenant(tenant).is_err() || validate_run(run).is_err() {
            return;
        }
        let key = RunKey::new(tenant, run);
        let now = unix_now_ns();
        let Ok(mut state) = guard.lock_state() else {
            return;
        };
        let Some(live) = state.runs.get_mut(&key) else {
            return;
        };
        live.pending = Some(LiveStep {
            kind: if next.tool.is_some() {
                StepKind::Tool
            } else {
                StepKind::Llm
            },
            name: next.tool.as_deref().map(crate::steer::handoff::clip_name),
            started_ns: now,
            pending: true,
        });
        state.live_touch(&key, now, true);
    }

    /// Re-stamps `run` in the live view after something a viewer sees
    /// changed outside the guard (its nudges). Nothing for a run that is not
    /// live.
    pub(crate) fn guard_touch_live(&self, tenant: &str, run: &str) {
        let Some(guard) = self.guard.as_ref() else {
            return;
        };
        let key = RunKey::new(tenant, run);
        if let Ok(mut state) = guard.lock_state() {
            if state.runs.contains_key(&key) {
                state.live_touch(&key, unix_now_ns(), false);
            }
        }
    }
}
