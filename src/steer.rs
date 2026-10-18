//! Steer: watch the runs that are live, speak to one before its next step,
//! and see how its agents hand control around.
//!
//! The run guard ([`crate::guard`]) already holds every live run's usage and
//! verdict in memory and is asked, before each step, whether the step may
//! run. Steering builds on exactly that, adding no store fold to any hot
//! path and no model anywhere:
//!
//! - **Live runs** — runs with activity (a span arriving, or a pre-call
//!   check) in the last `--live-window-seconds`, most recently changed
//!   first, each with its agent, the step it is on, its usage, its verdict
//!   and the nudges waiting for it: [`Store::live_runs`], and a stream of
//!   their changes, [`Store::live_wait`]. Built from guard live state alone.
//! - **Nudges** — a human's advice to a running agent, or a veto of its next
//!   step, delivered once through the run's next pre-call check — a
//!   `POST /v1/fuses/check` naming its `next` step ([`Store::create_nudge`];
//!   the delivery rules are in the `nudges` module, and [`check_reply`] is
//!   where a check carries them).
//! - **Topology** — a run's agents, who delegated to whom, who handed
//!   control to whom, and where two agents passed it back and forth
//!   ([`Store::run_topology`]). The guard's `max_handoff_cycles` limit holds
//!   a run to the same count of `A → B → A` returns ([`handoff`]).
//!
//! Live runs and nudges need the run guard: under `--no-fuses` they refuse,
//! as every fuse surface does. The topology reads the store and works
//! either way.

pub(crate) mod handoff;
pub mod http;
mod nudges;
mod topology;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

pub use crate::guard::{LiveCursor, LivePage, LiveRun, LiveStep, LiveUpdate};
pub use handoff::MAX_AGENT_NAME_CHARS;
pub use nudges::{
    Nudge, NudgeKind, NudgeSpec, NudgeStatus, BLOCK_HOLD_MS, DEFAULT_NUDGE_TTL_SECONDS,
    MAX_HELD_NUDGE_BYTES, MAX_KEPT_PER_RUN, MAX_NUDGE_BYTES, MAX_NUDGE_TTL_SECONDS,
    MAX_PENDING_PER_RUN, MAX_TENANT_NUDGE_BYTES, NUDGE_OVERHEAD_BYTES,
};
pub use topology::{
    Topology, TopologyCycle, TopologyEdge, TopologyHandoff, TopologyNode, MAX_CYCLES, MAX_EDGES,
    MAX_HANDOFFS, MAX_NODES, MAX_SEQUENCE, MAX_TOPOLOGY_SPANS,
};

use crate::guard::{Verdict, VerdictDoc};
use crate::{erasure, Error, Result, Store};

/// Default activity window of the live view (`--live-window-seconds`).
pub const DEFAULT_LIVE_WINDOW_SECONDS: u64 = 900;

/// Most live-run streams one server serves at once; one more is refused.
pub const MAX_LIVE_STREAMS: usize = 64;

/// Most of those one scope — one tenant, or every tenant — may hold, so a
/// single tenant's credential cannot take every slot.
pub const MAX_LIVE_STREAMS_PER_SCOPE: usize = 8;

/// Most runs one stream update carries as changes; more changing at once
/// restarts the stream from a listing instead.
pub const MAX_LIVE_CHANGES: usize = 256;

/// Steering's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteerConfig {
    /// How recent a run's last activity must be for it to be live
    /// (`--live-window-seconds`).
    pub live_window: Duration,
}

impl Default for SteerConfig {
    fn default() -> Self {
        Self {
            live_window: Duration::from_secs(DEFAULT_LIVE_WINDOW_SECONDS),
        }
    }
}

/// Live streams being served: in all, and per scope (a tenant, or `None`
/// for every tenant).
#[derive(Debug, Default)]
struct Streams {
    total: usize,
    by_scope: HashMap<Option<String>, usize>,
}

/// What steering keeps beside the guard: the nudges and their log, and the
/// live streams being served.
pub(crate) struct Steer {
    nudges: nudges::Nudges,
    streams: Mutex<Streams>,
}

impl Steer {
    /// Opens `steer.jsonl` in `directory`; `None` when run fuses are off,
    /// in which case the log is not even read.
    pub(crate) fn open(directory: &Path, fuses_enabled: bool) -> Result<Option<Self>> {
        if !fuses_enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            nudges: nudges::Nudges::open(directory, unix_now_ns())?,
            streams: Mutex::new(Streams::default()),
        }))
    }
}

/// A live stream's claim on one of [`MAX_LIVE_STREAMS`] slots — and one of
/// its scope's [`MAX_LIVE_STREAMS_PER_SCOPE`] — released on drop.
pub struct LiveStreamSlot<'a> {
    streams: &'a Mutex<Streams>,
    scope: Option<String>,
}

impl Drop for LiveStreamSlot<'_> {
    fn drop(&mut self) {
        // A poisoned lock means a panic mid-count; the slot stays taken,
        // which errs toward refusing a stream, never toward too many.
        if let Ok(mut streams) = self.streams.lock() {
            streams.total = streams.total.saturating_sub(1);
            if let Some(held) = streams.by_scope.get_mut(&self.scope) {
                *held = held.saturating_sub(1);
                if *held == 0 {
                    streams.by_scope.remove(&self.scope);
                }
            }
        }
    }
}

fn unix_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

impl Store {
    fn steer(&self) -> Result<&Steer> {
        self.steer.as_ref().ok_or_else(|| {
            Error::Conflict(
                "live runs and nudges need run fuses, which are disabled on this store \
                 (traza-server --no-fuses)"
                    .to_owned(),
            )
        })
    }

    /// Queues a nudge for `run` within `tenant` (empty: the default
    /// tenant), durably, for its next pre-call check to deliver. A run need
    /// not have started: a nudge can wait for its first check. Refused with
    /// a conflict while an erasure covering the run is pending — asked
    /// under the nudge log's lock, which the erasure's own forgetting takes
    /// too, so no nudge queued during an erasure outlives it.
    pub fn create_nudge(&self, tenant: &str, run: &str, spec: &NudgeSpec) -> Result<Nudge> {
        let steer = self.steer()?;
        crate::guard::validate_tenant(tenant)?;
        crate::guard::validate_run(run)?;
        let covered = || {
            self.erasure_mask()
                .is_some_and(|mask| mask.covers_run(tenant, run))
        };
        let nudge = steer
            .nudges
            .create(tenant, run, spec, unix_now_ns(), &covered)?;
        self.guard_touch_live(tenant, run);
        Ok(nudge)
    }

    /// `run`'s nudges — pending, delivered and expired — in the order they
    /// were queued.
    pub fn nudges(&self, tenant: &str, run: &str) -> Result<Vec<Nudge>> {
        let steer = self.steer()?;
        crate::guard::validate_tenant(tenant)?;
        crate::guard::validate_run(run)?;
        steer.nudges.list(tenant, run)
    }

    /// Delivers `run`'s pending nudges: marks them delivered, durably, and
    /// returns them, followed by the blocks of a delivery still holding;
    /// see [`check_reply`].
    pub(crate) fn deliver_nudges(&self, tenant: &str, run: &str) -> Result<Vec<Nudge>> {
        let Some(steer) = self.steer.as_ref() else {
            return Ok(Vec::new());
        };
        let now = unix_now_ns();
        let delivered = steer.nudges.deliver(tenant, run, now)?;
        if delivered
            .iter()
            .any(|nudge| nudge.delivered_at_ns == Some(now))
        {
            self.guard_touch_live(tenant, run);
        }
        Ok(delivered)
    }

    /// Live runs — activity in the last `--live-window-seconds` — of
    /// `tenant` (every tenant: `None`), most recently changed first, at
    /// most `limit`. From guard live state alone; see [`crate::guard`]'s
    /// live view for what it does and does not show.
    pub fn live_runs(&self, tenant: Option<&str>, limit: usize) -> Result<LivePage> {
        let steer = self.steer()?;
        let mut page = self.guard_live_page(tenant, limit, self.config.steer.live_window)?;
        fill_pending(steer, &mut page.runs)?;
        self.fill_live_foresight(&mut page.runs);
        Ok(page)
    }

    /// Waits up to `wait` for live runs to change after `cursor` and says
    /// what a stream should send next, advancing `cursor`: a listing to
    /// start from (the first call, after an erasure, or when more than
    /// [`MAX_LIVE_CHANGES`] runs changed at once), the runs that changed, or
    /// nothing.
    pub fn live_wait(
        &self,
        tenant: Option<&str>,
        cursor: &mut LiveCursor,
        wait: Duration,
    ) -> Result<LiveUpdate> {
        let steer = self.steer()?;
        let mut update = self.guard_live_wait(
            tenant,
            cursor,
            MAX_LIVE_CHANGES,
            self.config.steer.live_window,
            wait,
        )?;
        match &mut update {
            LiveUpdate::Snapshot(page) => {
                fill_pending(steer, &mut page.runs)?;
                self.fill_live_foresight(&mut page.runs);
            }
            LiveUpdate::Changed(runs) => {
                fill_pending(steer, runs)?;
                self.fill_live_foresight(runs);
            }
            LiveUpdate::Idle => {}
        }
        Ok(update)
    }

    /// Claims one of [`MAX_LIVE_STREAMS`] stream slots for a stream of
    /// `scope` (a tenant, or `None` for every tenant), which may hold at
    /// most [`MAX_LIVE_STREAMS_PER_SCOPE`] of them; a conflict when either
    /// is full or steering is off.
    pub fn live_stream_slot(&self, scope: Option<&str>) -> Result<LiveStreamSlot<'_>> {
        let steer = self.steer()?;
        let mut streams = steer
            .streams
            .lock()
            .map_err(|_| Error::LockPoisoned("live streams"))?;
        let scope = scope.map(str::to_owned);
        if streams.total >= MAX_LIVE_STREAMS {
            return Err(Error::Conflict(format!(
                "this server is already serving {MAX_LIVE_STREAMS} live-run streams; close one, \
                 or poll GET /v1/runs/live"
            )));
        }
        if streams.by_scope.get(&scope).copied().unwrap_or(0) >= MAX_LIVE_STREAMS_PER_SCOPE {
            return Err(Error::Conflict(format!(
                "{} already has {MAX_LIVE_STREAMS_PER_SCOPE} live-run streams open; close one, \
                 or poll GET /v1/runs/live",
                if scope.is_some() {
                    "this tenant"
                } else {
                    "the every-tenant view"
                }
            )));
        }
        streams.total += 1;
        *streams.by_scope.entry(scope.clone()).or_default() += 1;
        Ok(LiveStreamSlot {
            streams: &steer.streams,
            scope,
        })
    }

    /// The activity window of the live view.
    pub fn live_window(&self) -> Duration {
        self.config.steer.live_window
    }

    /// Removes the nudges an erasure subject names: a session's or trace's
    /// run, or a whole tenant's. Spans and payloads name no run of their
    /// own. Idempotent.
    pub(crate) fn forget_steer_subject(&self, subject: &erasure::Subject) -> Result<()> {
        let Some(steer) = self.steer.as_ref() else {
            return Ok(());
        };
        let nudges = &steer.nudges;
        match subject {
            erasure::Subject::Session { session_id, tenant } => {
                nudges.forget(tenant, Some(session_id))
            }
            erasure::Subject::Trace { trace_id, tenant } => nudges.forget(
                tenant,
                Some(&format!("{}{trace_id}", crate::semconv::TRACE_RUN_PREFIX)),
            ),
            erasure::Subject::Tenant { tenant } => nudges.forget(tenant, None),
            _ => Ok(()),
        }
    }
}

/// Every row's pending nudge count, under one acquisition of the book.
fn fill_pending(steer: &Steer, runs: &mut [LiveRun]) -> Result<()> {
    if runs.is_empty() {
        return Ok(());
    }
    let counts = steer.nudges.pending_each(
        runs.iter()
            .map(|run| (run.tenant.as_str(), run.run.as_str())),
        unix_now_ns(),
    )?;
    for (run, count) in runs.iter_mut().zip(counts) {
        run.pending_nudges = count;
    }
    Ok(())
}

/// The body `POST /v1/fuses/check` answers: the verdict document `doc`,
/// plus — on a pre-call check (`pre_step`: the request named its `next`
/// step) — every nudge pending for the run, delivered now, durably, so each
/// is delivered at most once, as `"nudges": [{id, kind, message}]`. A check
/// naming no next step is a verdict read: it delivers nothing and answers
/// `"nudges": []`, so a probe or a dashboard cannot take an agent's nudges.
///
/// A `block` nudge is a human veto: the answer is `stop`, with the blocking
/// messages as its first reasons (the guard's own follow, if it said `stop`
/// too), and nothing about the run's fuse changes — no trip, no event, no
/// sticky stop. Every pre-call check of the run in the [`BLOCK_HOLD_MS`]
/// after the delivery is vetoed the same way, carrying the same block nudge
/// again, so the parallel tool calls of one batch are all refused; after
/// that the guard judges the run as usual. If the delivery cannot be
/// recorded, the check fails and every nudge stays pending.
pub(crate) fn check_reply(
    store: &Store,
    tenant: &str,
    run: &str,
    doc: &VerdictDoc,
    pre_step: bool,
) -> Result<Value> {
    let delivered = if pre_step {
        store.deliver_nudges(tenant, run)?
    } else {
        Vec::new()
    };
    let mut body = serde_json::to_value(doc)?;
    let blocks: Vec<String> = delivered
        .iter()
        .filter(|nudge| nudge.kind == NudgeKind::Block)
        .map(|nudge| nudge.message.clone())
        .collect();
    if let Some(object) = body.as_object_mut() {
        if !blocks.is_empty() {
            let mut reasons = blocks;
            if doc.verdict == Verdict::Stop {
                reasons.extend(doc.reasons.iter().cloned());
            }
            object.insert("verdict".to_owned(), json!(Verdict::Stop));
            object.insert("reasons".to_owned(), json!(reasons));
        }
        let nudges: Vec<Value> = delivered
            .iter()
            .map(|nudge| json!({"id": nudge.id, "kind": nudge.kind, "message": nudge.message}))
            .collect();
        object.insert("nudges".to_owned(), Value::Array(nudges));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_streams_are_bounded_and_a_dropped_one_frees_its_slot() {
        let dir = std::env::temp_dir().join(format!(
            "traza-steer-slots-{}-{}",
            std::process::id(),
            unix_now_ns()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let store = Store::open(&dir, crate::Config::default()).expect("opens");
        // One scope holds at most its share; the rest stay open to others.
        let mut slots: Vec<LiveStreamSlot<'_>> = (0..MAX_LIVE_STREAMS_PER_SCOPE)
            .map(|_| store.live_stream_slot(Some("acme")).expect("a free slot"))
            .collect();
        match store.live_stream_slot(Some("acme")) {
            Err(Error::Conflict(message)) => assert!(
                message.contains("this tenant already has 8 live-run streams"),
                "{message}"
            ),
            other => panic!(
                "acme's ninth stream must be refused, got {:?}",
                other.is_ok()
            ),
        }
        slots.pop();
        slots.push(
            store
                .live_stream_slot(Some("acme"))
                .expect("a released slot is free again"),
        );
        let mut opened = 0;
        while slots.len() < MAX_LIVE_STREAMS {
            let scope = format!("t{}", opened / MAX_LIVE_STREAMS_PER_SCOPE);
            slots.push(store.live_stream_slot(Some(&scope)).expect("a free slot"));
            opened += 1;
        }
        match store.live_stream_slot(None) {
            Err(Error::Conflict(message)) => {
                assert!(message.contains("64 live-run streams"), "{message}")
            }
            other => panic!("the 65th stream must be refused, got {:?}", other.is_ok()),
        }
        slots.pop();
        let again = store
            .live_stream_slot(None)
            .expect("a released slot is free again");
        drop(again);
        drop(slots);
        let streams = store.steer().expect("steer").streams.lock().expect("lock");
        assert_eq!(streams.total, 0, "every slot released");
        assert!(streams.by_scope.is_empty(), "{:?}", streams.by_scope);
        drop(streams);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
