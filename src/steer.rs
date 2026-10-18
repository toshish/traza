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
//!   step, delivered once through the run's next `POST /v1/fuses/check`
//!   ([`Store::create_nudge`]; the delivery rules are in the `nudges`
//!   module, and [`check_reply`] is where a check carries them).
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

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};

pub use crate::guard::{LiveCursor, LivePage, LiveRun, LiveStep, LiveUpdate};
pub use handoff::MAX_AGENT_NAME_CHARS;
pub use nudges::{
    Nudge, NudgeKind, NudgeSpec, NudgeStatus, DEFAULT_NUDGE_TTL_SECONDS, MAX_KEPT_PER_RUN,
    MAX_NUDGED_RUNS_PER_TENANT, MAX_NUDGE_BYTES, MAX_NUDGE_TTL_SECONDS, MAX_PENDING_PER_RUN,
};
pub use topology::{
    Topology, TopologyCycle, TopologyEdge, TopologyHandoff, TopologyNode, MAX_CYCLES, MAX_EDGES,
    MAX_NODES, MAX_SEQUENCE, MAX_TOPOLOGY_SPANS,
};

use crate::guard::{Verdict, VerdictDoc};
use crate::{erasure, Error, Result, Store};

/// Default activity window of the live view (`--live-window-seconds`).
pub const DEFAULT_LIVE_WINDOW_SECONDS: u64 = 900;

/// Most live-run streams one server serves at once; one more is refused.
pub const MAX_LIVE_STREAMS: usize = 64;

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

/// What steering keeps beside the guard: the nudges and their log, and a
/// count of live streams being served.
pub(crate) struct Steer {
    book: Mutex<nudges::Book>,
    streams: AtomicUsize,
}

impl Steer {
    /// Opens `steer.jsonl` in `directory`; `None` when run fuses are off,
    /// in which case the log is not even read.
    pub(crate) fn open(directory: &Path, fuses_enabled: bool) -> Result<Option<Self>> {
        if !fuses_enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            book: Mutex::new(nudges::Book::open(directory, unix_now_ns())?),
            streams: AtomicUsize::new(0),
        }))
    }

    fn lock_book(&self) -> Result<MutexGuard<'_, nudges::Book>> {
        self.book.lock().map_err(|_| Error::LockPoisoned("nudges"))
    }
}

/// A live stream's claim on one of [`MAX_LIVE_STREAMS`] slots, released on
/// drop.
pub struct LiveStreamSlot<'a> {
    streams: &'a AtomicUsize,
}

impl Drop for LiveStreamSlot<'_> {
    fn drop(&mut self) {
        self.streams.fetch_sub(1, Ordering::AcqRel);
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
    /// a conflict while an erasure covering the run is pending.
    pub fn create_nudge(&self, tenant: &str, run: &str, spec: &NudgeSpec) -> Result<Nudge> {
        let steer = self.steer()?;
        crate::guard::validate_tenant(tenant)?;
        crate::guard::validate_run(run)?;
        if self
            .erasure_mask()
            .is_some_and(|mask| mask.covers_run(tenant, run))
        {
            return Err(Error::Conflict(format!(
                "an erasure covering run `{run}` is pending; nudge it once the erasure settles"
            )));
        }
        let nudge = steer
            .lock_book()?
            .create(tenant, run, spec, unix_now_ns())?;
        self.guard_touch_live(tenant, run);
        Ok(nudge)
    }

    /// `run`'s nudges — pending, delivered and expired — in the order they
    /// were queued.
    pub fn nudges(&self, tenant: &str, run: &str) -> Result<Vec<Nudge>> {
        let steer = self.steer()?;
        crate::guard::validate_tenant(tenant)?;
        crate::guard::validate_run(run)?;
        Ok(steer.lock_book()?.list(tenant, run))
    }

    /// Delivers `run`'s pending nudges: marks them delivered, durably, and
    /// returns them; see [`check_reply`].
    pub(crate) fn deliver_nudges(&self, tenant: &str, run: &str) -> Result<Vec<Nudge>> {
        let Some(steer) = self.steer.as_ref() else {
            return Ok(Vec::new());
        };
        let delivered = steer.lock_book()?.deliver(tenant, run, unix_now_ns())?;
        if !delivered.is_empty() {
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
            LiveUpdate::Snapshot(page) => fill_pending(steer, &mut page.runs)?,
            LiveUpdate::Changed(runs) => fill_pending(steer, runs)?,
            LiveUpdate::Idle => {}
        }
        Ok(update)
    }

    /// Claims one of [`MAX_LIVE_STREAMS`] stream slots; a conflict when all
    /// are taken or steering is off.
    pub fn live_stream_slot(&self) -> Result<LiveStreamSlot<'_>> {
        let steer = self.steer()?;
        let taken = steer.streams.fetch_add(1, Ordering::AcqRel);
        let slot = LiveStreamSlot {
            streams: &steer.streams,
        };
        if taken >= MAX_LIVE_STREAMS {
            drop(slot);
            return Err(Error::Conflict(format!(
                "this server is already serving {MAX_LIVE_STREAMS} live-run streams; close one, \
                 or poll GET /v1/runs/live"
            )));
        }
        Ok(slot)
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
        let mut book = steer.lock_book()?;
        match subject {
            erasure::Subject::Session { session_id, tenant } => {
                book.forget(tenant, Some(session_id))
            }
            erasure::Subject::Trace { trace_id, tenant } => book.forget(
                tenant,
                Some(&format!("{}{trace_id}", crate::semconv::TRACE_RUN_PREFIX)),
            ),
            erasure::Subject::Tenant { tenant } => book.forget(tenant, None),
            _ => Ok(()),
        }
    }
}

/// Every row's pending nudge count, under one acquisition of the book.
fn fill_pending(steer: &Steer, runs: &mut [LiveRun]) -> Result<()> {
    if runs.is_empty() {
        return Ok(());
    }
    let now = unix_now_ns();
    let book = steer.lock_book()?;
    for run in runs {
        run.pending_nudges = book.pending(&run.tenant, &run.run, now);
    }
    Ok(())
}

/// The body `POST /v1/fuses/check` answers: the verdict document `doc`,
/// plus every nudge pending for the run — delivered now, durably, so each
/// reaches exactly one check — as `"nudges": [{id, kind, message}]`.
///
/// A `block` nudge is a human veto: the answer is `stop`, with the blocking
/// messages as its first reasons (the guard's own follow, if it said `stop`
/// too), and nothing about the run's fuse changes — no trip, no event, no
/// sticky stop — so the run's next check is judged by the guard as usual.
/// If the delivery cannot be recorded, the check fails and every nudge
/// stays pending.
pub(crate) fn check_reply(
    store: &Store,
    tenant: &str,
    run: &str,
    doc: &VerdictDoc,
) -> Result<Value> {
    let delivered = store.deliver_nudges(tenant, run)?;
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
        let mut slots: Vec<LiveStreamSlot<'_>> = (0..MAX_LIVE_STREAMS)
            .map(|_| store.live_stream_slot().expect("a free slot"))
            .collect();
        match store.live_stream_slot() {
            Err(Error::Conflict(message)) => {
                assert!(message.contains("64 live-run streams"), "{message}")
            }
            other => panic!("the 65th stream must be refused, got {:?}", other.is_ok()),
        }
        slots.pop();
        let again = store
            .live_stream_slot()
            .expect("a released slot is free again");
        drop(again);
        drop(slots);
        assert_eq!(
            store
                .steer()
                .expect("steer")
                .streams
                .load(Ordering::Acquire),
            0,
            "every slot released"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
