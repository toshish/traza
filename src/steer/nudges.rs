//! Nudges: a human's word to a running agent, delivered once, before its
//! next step.
//!
//! An operator watching a run queues a nudge — `advice` the agent should
//! hear, or a `block` that vetoes its next step — and the run's next
//! pre-call check (`POST /v1/fuses/check`) carries every pending nudge in
//! its verdict and marks them delivered, durably, before it answers: each
//! nudge reaches at most one check. A `block` makes that one check answer
//! `stop` with the human's message as the reason, without tripping the fuse,
//! so the step after it is judged by the guard as usual.
//!
//! # Durable, bounded, erasable
//!
//! Nudges live in `steer.jsonl` beside `fuses.jsonl`, written through the
//! same log ([`crate::guard`]'s): one fsynced line per nudge and per
//! delivery, a torn final line healed at open, damage anywhere else refusing
//! startup, rewritten to the surviving state when at least half its lines
//! are superseded. Like `fuses.jsonl` it is operator state outside the
//! generation layout: a backup does not carry it. Bounds, all per tenant:
//!
//! - a message is 1 to [`MAX_NUDGE_BYTES`] bytes;
//! - a run holds at most [`MAX_PENDING_PER_RUN`] pending nudges (one more is
//!   `409`) and keeps at most [`MAX_KEPT_PER_RUN`] in all, delivered and
//!   expired ones dropped oldest first;
//! - a tenant holds nudges for at most [`MAX_NUDGED_RUNS_PER_TENANT`] runs:
//!   past it, the history of the run nudged longest ago with nothing pending
//!   is dropped, and when every run has something pending, a new one is
//!   refused with `409`.
//!
//! An erasure of a run's session or trace, or of its tenant, removes its
//! nudges and rewrites the log without them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::guard::log::FuseLog;
use crate::{Error, Result};

/// The log's file name in the data directory.
pub(crate) const LOG_NAME: &str = "steer.jsonl";

/// Longest nudge message, in bytes.
pub const MAX_NUDGE_BYTES: usize = 4096;

/// Most pending nudges one run may hold.
pub const MAX_PENDING_PER_RUN: usize = 32;

/// Most nudges one run keeps, pending or not.
pub const MAX_KEPT_PER_RUN: usize = 64;

/// Most runs one tenant holds nudges for.
pub const MAX_NUDGED_RUNS_PER_TENANT: usize = 10_000;

/// How long a nudge waits for a check when the request names no
/// `ttl_seconds`: an hour.
pub const DEFAULT_NUDGE_TTL_SECONDS: u64 = 3600;

/// The longest `ttl_seconds` accepted: seven days.
pub const MAX_NUDGE_TTL_SECONDS: u64 = 7 * 24 * 3600;

/// What a nudge asks of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NudgeKind {
    /// Words for the agent; its next step proceeds.
    Advice,
    /// A veto: the check that delivers it answers `stop`, with the message
    /// as the reason. It vetoes that one step; it does not trip the fuse.
    Block,
}

/// Where a nudge stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NudgeStatus {
    /// Waiting for the run's next pre-call check.
    Pending,
    /// Carried by a check's verdict.
    Delivered,
    /// Its time ran out before a check asked.
    Expired,
}

/// One nudge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Nudge {
    /// `nd_` and 16 lowercase hex characters.
    pub id: String,
    /// The run's tenant; empty is the default tenant.
    #[serde(default)]
    pub tenant: String,
    /// The run it is for.
    pub run: String,
    /// Advice or a veto.
    pub kind: NudgeKind,
    /// What the human wrote. Operator text, delivered to the agent as is.
    pub message: String,
    /// When it was queued, server clock.
    pub created_at_ns: u64,
    /// When it stops waiting, server clock.
    pub expires_at_ns: u64,
    /// When a check delivered it; `null` until then.
    pub delivered_at_ns: Option<u64>,
}

impl Nudge {
    /// Where it stands at `now`.
    pub fn status(&self, now: u64) -> NudgeStatus {
        match self.delivered_at_ns {
            Some(_) => NudgeStatus::Delivered,
            None if now >= self.expires_at_ns => NudgeStatus::Expired,
            None => NudgeStatus::Pending,
        }
    }
}

/// A nudge as submitted.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NudgeSpec {
    /// What to tell the agent.
    pub message: String,
    /// Advice or a veto.
    pub kind: NudgeKind,
    /// Seconds it waits for a check: 1 to [`MAX_NUDGE_TTL_SECONDS`],
    /// default [`DEFAULT_NUDGE_TTL_SECONDS`].
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

impl NudgeSpec {
    fn validate(&self) -> Result<()> {
        if self.message.trim().is_empty() {
            return Err(Error::Invalid("message must not be empty".to_owned()));
        }
        if self.message.len() > MAX_NUDGE_BYTES {
            return Err(Error::Invalid(format!(
                "message must be at most {MAX_NUDGE_BYTES} bytes (got {})",
                self.message.len()
            )));
        }
        if let Some(ttl) = self.ttl_seconds {
            if ttl == 0 || ttl > MAX_NUDGE_TTL_SECONDS {
                return Err(Error::Invalid(format!(
                    "ttl_seconds must be 1 to {MAX_NUDGE_TTL_SECONDS} (got {ttl})"
                )));
            }
        }
        Ok(())
    }
}

/// One durable nudge fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Record {
    /// A nudge queued — or, in a rewritten log, a nudge as it stands.
    Nudge {
        /// The nudge.
        nudge: Nudge,
    },
    /// Nudges a check delivered.
    Deliver {
        /// The run's tenant.
        #[serde(default)]
        tenant: String,
        /// The run.
        run: String,
        /// The delivered nudges, in the order they were delivered.
        ids: Vec<String>,
        /// When, server clock.
        at_ns: u64,
    },
}

/// One run's nudges, in the order they were queued.
#[derive(Clone, Debug, Default)]
struct RunNudges {
    nudges: Vec<Nudge>,
}

impl RunNudges {
    fn pending(&self, now: u64) -> usize {
        self.nudges
            .iter()
            .filter(|nudge| nudge.status(now) == NudgeStatus::Pending)
            .count()
    }

    /// The latest time anything happened to the run's nudges.
    fn last_ns(&self) -> u64 {
        self.nudges
            .iter()
            .map(|nudge| nudge.delivered_at_ns.unwrap_or(nudge.created_at_ns))
            .max()
            .unwrap_or(0)
    }

    /// Drops delivered and expired nudges, oldest first, beyond the bound.
    fn trim(&mut self, now: u64) {
        let mut excess = self.nudges.len().saturating_sub(MAX_KEPT_PER_RUN);
        if excess == 0 {
            return;
        }
        self.nudges.retain(|nudge| {
            if excess > 0 && nudge.status(now) != NudgeStatus::Pending {
                excess -= 1;
                return false;
            }
            true
        });
    }
}

/// Every tenant's nudges, and the log that makes them durable. One lock
/// covers both: a nudge is queued or delivered rarely, and a check with
/// nothing pending holds it for one map lookup.
pub(crate) struct Book {
    runs: HashMap<String, HashMap<String, RunNudges>>,
    log: FuseLog<Record>,
}

impl Book {
    /// Opens `steer.jsonl` in `directory` and replays it.
    pub(crate) fn open(directory: &Path, now: u64) -> Result<Self> {
        let (log, records) = FuseLog::<Record>::open_named(directory, LOG_NAME)?;
        let mut book = Self {
            runs: HashMap::new(),
            log,
        };
        for record in records {
            book.apply(record);
        }
        for runs in book.runs.values_mut() {
            for nudges in runs.values_mut() {
                nudges.trim(now);
            }
        }
        book.trim_tenants(now);
        book.compact_if_due()?;
        Ok(book)
    }

    fn apply(&mut self, record: Record) {
        match record {
            Record::Nudge { nudge } => {
                let nudges = &mut self
                    .runs
                    .entry(nudge.tenant.clone())
                    .or_default()
                    .entry(nudge.run.clone())
                    .or_default()
                    .nudges;
                match nudges.iter_mut().find(|held| held.id == nudge.id) {
                    Some(held) => *held = nudge,
                    None => nudges.push(nudge),
                }
            }
            Record::Deliver {
                tenant,
                run,
                ids,
                at_ns,
            } => {
                if let Some(nudges) = self
                    .runs
                    .get_mut(&tenant)
                    .and_then(|runs| runs.get_mut(&run))
                {
                    for nudge in &mut nudges.nudges {
                        if ids.contains(&nudge.id) && nudge.delivered_at_ns.is_none() {
                            nudge.delivered_at_ns = Some(at_ns);
                        }
                    }
                }
            }
        }
    }

    /// Brings every tenant within [`MAX_NUDGED_RUNS_PER_TENANT`] after a
    /// replay, dropping the runs nudged longest ago with nothing pending.
    fn trim_tenants(&mut self, now: u64) {
        for runs in self.runs.values_mut() {
            while runs.len() > MAX_NUDGED_RUNS_PER_TENANT {
                let Some(oldest) = oldest_settled(runs, now) else {
                    break;
                };
                runs.remove(&oldest);
            }
        }
    }

    /// The records that reproduce the state, for a rewrite.
    fn records(&self) -> Vec<Record> {
        let mut nudges: Vec<&Nudge> = self
            .runs
            .values()
            .flat_map(HashMap::values)
            .flat_map(|run| run.nudges.iter())
            .collect();
        nudges.sort_by(|left, right| {
            (left.created_at_ns, &left.id).cmp(&(right.created_at_ns, &right.id))
        });
        nudges
            .into_iter()
            .map(|nudge| Record::Nudge {
                nudge: nudge.clone(),
            })
            .collect()
    }

    fn compact_if_due(&mut self) -> Result<()> {
        if self.log.compaction_due() {
            let records = self.records();
            self.log.compact(&records)?;
        }
        Ok(())
    }

    /// Queues a nudge for `run`, durably.
    pub(crate) fn create(
        &mut self,
        tenant: &str,
        run: &str,
        spec: &NudgeSpec,
        now: u64,
    ) -> Result<Nudge> {
        spec.validate()?;
        let pending = self
            .runs
            .get(tenant)
            .and_then(|runs| runs.get(run))
            .map_or(0, |nudges| nudges.pending(now));
        if pending >= MAX_PENDING_PER_RUN {
            return Err(Error::Conflict(format!(
                "run `{run}` already has {MAX_PENDING_PER_RUN} pending nudges; wait for its next \
                 check to deliver them, or for them to expire"
            )));
        }
        let make_room = match self.runs.get(tenant) {
            Some(runs) if !runs.contains_key(run) && runs.len() >= MAX_NUDGED_RUNS_PER_TENANT => {
                match oldest_settled(runs, now) {
                    Some(oldest) => Some(oldest),
                    None => {
                        return Err(Error::Conflict(format!(
                            "this tenant already has pending nudges for \
                             {MAX_NUDGED_RUNS_PER_TENANT} runs; wait for them to be delivered \
                             or to expire"
                        )))
                    }
                }
            }
            _ => None,
        };
        let ttl = spec.ttl_seconds.unwrap_or(DEFAULT_NUDGE_TTL_SECONDS);
        let nudge = Nudge {
            id: nudge_id(tenant, run, now),
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            kind: spec.kind,
            message: spec.message.clone(),
            created_at_ns: now,
            expires_at_ns: now.saturating_add(ttl.saturating_mul(1_000_000_000)),
            delivered_at_ns: None,
        };
        self.log.append(&Record::Nudge {
            nudge: nudge.clone(),
        })?;
        let runs = self.runs.entry(tenant.to_owned()).or_default();
        if let Some(oldest) = make_room {
            runs.remove(&oldest);
        }
        let nudges = runs.entry(run.to_owned()).or_default();
        nudges.nudges.push(nudge.clone());
        nudges.trim(now);
        self.compact_if_due()?;
        Ok(nudge)
    }

    /// `run`'s nudges, in the order they were queued.
    pub(crate) fn list(&self, tenant: &str, run: &str) -> Vec<Nudge> {
        self.runs
            .get(tenant)
            .and_then(|runs| runs.get(run))
            .map(|nudges| nudges.nudges.clone())
            .unwrap_or_default()
    }

    /// Nudges pending for `run` at `now`.
    pub(crate) fn pending(&self, tenant: &str, run: &str, now: u64) -> usize {
        self.runs
            .get(tenant)
            .and_then(|runs| runs.get(run))
            .map_or(0, |nudges| nudges.pending(now))
    }

    /// Delivers `run`'s pending nudges: marks them delivered, durably, and
    /// returns them in the order they were queued. Nothing pending costs a
    /// lookup and writes nothing. If the delivery record cannot be written,
    /// nothing is delivered and the nudges stay pending.
    pub(crate) fn deliver(&mut self, tenant: &str, run: &str, now: u64) -> Result<Vec<Nudge>> {
        let Some(nudges) = self.runs.get_mut(tenant).and_then(|runs| runs.get_mut(run)) else {
            return Ok(Vec::new());
        };
        let due: Vec<String> = nudges
            .nudges
            .iter()
            .filter(|nudge| nudge.status(now) == NudgeStatus::Pending)
            .map(|nudge| nudge.id.clone())
            .collect();
        if due.is_empty() {
            return Ok(Vec::new());
        }
        self.log.append(&Record::Deliver {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            ids: due.clone(),
            at_ns: now,
        })?;
        let mut delivered = Vec::with_capacity(due.len());
        if let Some(nudges) = self.runs.get_mut(tenant).and_then(|runs| runs.get_mut(run)) {
            for nudge in &mut nudges.nudges {
                if due.contains(&nudge.id) {
                    nudge.delivered_at_ns = Some(now);
                    delivered.push(nudge.clone());
                }
            }
        }
        self.compact_if_due()?;
        Ok(delivered)
    }

    /// Removes the nudges of one run (`Some(run)`) or of the whole tenant,
    /// and rewrites the log from what remains — always, since superseded
    /// lines can name the subject when memory no longer does.
    pub(crate) fn forget(&mut self, tenant: &str, run: Option<&str>) -> Result<()> {
        match run {
            Some(run) => {
                if let Some(runs) = self.runs.get_mut(tenant) {
                    runs.remove(run);
                    if runs.is_empty() {
                        self.runs.remove(tenant);
                    }
                }
            }
            None => {
                self.runs.remove(tenant);
            }
        }
        let records = self.records();
        self.log.rewrite(&records)
    }
}

/// The run nudged longest ago with nothing pending, if any.
fn oldest_settled(runs: &HashMap<String, RunNudges>, now: u64) -> Option<String> {
    runs.iter()
        .filter(|(_, nudges)| nudges.pending(now) == 0)
        .min_by(|left, right| (left.1.last_ns(), left.0).cmp(&(right.1.last_ns(), right.0)))
        .map(|(run, _)| run.clone())
}

/// A fresh nudge id: `nd_` and 16 hex characters, unique per process and
/// across restarts (the clock and a process-wide counter both feed it).
fn nudge_id(tenant: &str, run: &str, now: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("{tenant}\u{0}{run}\u{0}{now}\u{0}{serial}");
    let digest = crate::hash::hash128(material.as_bytes());
    let hex: String = digest.as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("nd_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "traza-nudges-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("dir");
        path
    }

    fn spec(kind: NudgeKind, message: &str) -> NudgeSpec {
        NudgeSpec {
            message: message.into(),
            kind,
            ttl_seconds: None,
        }
    }

    #[test]
    fn a_nudge_is_delivered_once_and_survives_a_restart_as_delivered() {
        let path = dir("once");
        let mut book = Book::open(&path, 1).expect("opens");
        let advice = book
            .create("", "run-1", &spec(NudgeKind::Advice, "use the cache"), 10)
            .expect("queues");
        assert!(advice.id.starts_with("nd_") && advice.id.len() == 19);
        assert_eq!(
            advice.expires_at_ns,
            10 + DEFAULT_NUDGE_TTL_SECONDS * 1_000_000_000
        );
        assert_eq!(book.pending("", "run-1", 11), 1);
        let delivered = book.deliver("", "run-1", 20).expect("delivers");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].delivered_at_ns, Some(20));
        assert!(
            book.deliver("", "run-1", 21).expect("nothing").is_empty(),
            "a nudge reaches one check"
        );
        drop(book);
        let book = Book::open(&path, 30).expect("reopens");
        let listed = book.list("", "run-1");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status(30), NudgeStatus::Delivered);
        assert_eq!(listed[0].delivered_at_ns, Some(20));
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn expired_nudges_are_not_delivered() {
        let path = dir("expiry");
        let mut book = Book::open(&path, 1).expect("opens");
        let short = NudgeSpec {
            ttl_seconds: Some(1),
            ..spec(NudgeKind::Block, "stop")
        };
        book.create("", "r", &short, 0).expect("queues");
        let later = 2_000_000_000;
        assert!(book.deliver("", "r", later).expect("none").is_empty());
        assert_eq!(book.list("", "r")[0].status(later), NudgeStatus::Expired);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn bounds_are_enforced_and_say_why() {
        let path = dir("bounds");
        let mut book = Book::open(&path, 1).expect("opens");
        for (bad, needle) in [
            (spec(NudgeKind::Advice, "   "), "must not be empty"),
            (
                spec(NudgeKind::Advice, &"x".repeat(MAX_NUDGE_BYTES + 1)),
                "at most 4096 bytes",
            ),
            (
                NudgeSpec {
                    ttl_seconds: Some(0),
                    ..spec(NudgeKind::Advice, "x")
                },
                "ttl_seconds must be 1",
            ),
        ] {
            match book.create("", "r", &bad, 1) {
                Err(Error::Invalid(message)) => assert!(message.contains(needle), "{message}"),
                other => panic!("expected {needle}, got {other:?}"),
            }
        }
        book.create(
            "",
            "r",
            &spec(NudgeKind::Advice, &"x".repeat(MAX_NUDGE_BYTES)),
            1,
        )
        .expect("exactly the cap fits");
        for _ in 1..MAX_PENDING_PER_RUN {
            book.create("", "r", &spec(NudgeKind::Advice, "x"), 1)
                .expect("queues");
        }
        match book.create("", "r", &spec(NudgeKind::Advice, "x"), 1) {
            Err(Error::Conflict(message)) => assert!(message.contains("32 pending"), "{message}"),
            other => panic!("the 33rd pending nudge must be refused, got {other:?}"),
        }
        // Delivered ones make room, and history stays bounded.
        book.deliver("", "r", 2).expect("delivers");
        for _ in 0..(MAX_KEPT_PER_RUN * 2) {
            book.create("", "r", &spec(NudgeKind::Advice, "x"), 3)
                .expect("queues");
            book.deliver("", "r", 4).expect("delivers");
        }
        assert!(book.list("", "r").len() <= MAX_KEPT_PER_RUN);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn forgetting_rewrites_the_log_without_the_subject() {
        let path = dir("forget");
        let mut book = Book::open(&path, 1).expect("opens");
        book.create("acme", "secret-session", &spec(NudgeKind::Advice, "a"), 1)
            .expect("queues");
        book.create("acme", "kept", &spec(NudgeKind::Advice, "b"), 1)
            .expect("queues");
        book.create("other", "x", &spec(NudgeKind::Advice, "c"), 1)
            .expect("queues");
        book.forget("acme", Some("secret-session"))
            .expect("forgets");
        let text = std::fs::read_to_string(path.join(LOG_NAME)).expect("reads");
        assert!(!text.contains("secret-session"), "{text}");
        assert!(text.contains("kept"));
        book.forget("acme", None).expect("forgets the tenant");
        let text = std::fs::read_to_string(path.join(LOG_NAME)).expect("reads");
        assert!(!text.contains("\"acme\""), "{text}");
        assert_eq!(book.list("other", "x").len(), 1);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn a_torn_final_line_is_healed_at_open() {
        let path = dir("torn");
        let mut book = Book::open(&path, 1).expect("opens");
        book.create("", "r", &spec(NudgeKind::Advice, "whole"), 1)
            .expect("queues");
        drop(book);
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path.join(LOG_NAME))
            .expect("opens the log");
        file.write_all(b"{\"op\":\"nudge\",\"nud").expect("tears");
        drop(file);
        let book = Book::open(&path, 2).expect("heals the torn tail");
        assert_eq!(book.list("", "r").len(), 1);
        let _ = std::fs::remove_dir_all(&path);
    }
}
