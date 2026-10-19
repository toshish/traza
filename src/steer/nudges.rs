//! Nudges: a human's word to a running agent, delivered once, before its
//! next step.
//!
//! An operator watching a run queues a nudge — `advice` the agent should
//! hear, or a `block` that vetoes its next step — and the run's next
//! pre-call check (`POST /v1/fuses/check` naming its `next` step) carries
//! every pending nudge in its verdict and marks them delivered, durably,
//! before it answers: each nudge is delivered at most once. A `block` makes
//! that check answer `stop` with the human's message as the reason, without
//! tripping the fuse — and so does every other pre-call check of the run in
//! the [`BLOCK_HOLD_MS`] after it, so the parallel tool calls of the batch
//! the veto lands in are all refused, not just the first to ask.
//!
//! # Durable, bounded, erasable
//!
//! Nudges live in `steer.jsonl` beside `fuses.jsonl`, written through the
//! same log ([`crate::guard`]'s): one fsynced line per nudge and per
//! delivery, a torn final line healed at open, damage anywhere else refusing
//! startup, rewritten to the surviving state when at least half its lines
//! are superseded. Like `fuses.jsonl` it is operator state outside the
//! generation layout: a backup does not carry it. Bounds:
//!
//! - a message is 1 to [`MAX_NUDGE_BYTES`] bytes;
//! - a run holds at most [`MAX_PENDING_PER_RUN`] pending nudges (one more is
//!   `409`) and keeps at most [`MAX_KEPT_PER_RUN`] in all, delivered and
//!   expired ones dropped oldest first;
//! - a tenant's nudges cost at most [`MAX_TENANT_NUDGE_BYTES`], and every
//!   tenant's together at most [`MAX_HELD_NUDGE_BYTES`] — a nudge costing
//!   its message, id, run id and tenant plus [`NUDGE_OVERHEAD_BYTES`]. Past
//!   either, delivered and expired nudges are dropped oldest first (the
//!   tenant's own for its budget, anyone's for the store's); when only
//!   pending ones are left to drop, a new nudge is refused with `409`;
//! - a block whose hold is still in force is a veto the run's checks are
//!   answered with, so neither the per-run trim nor a budget drops it until
//!   its hold ends; a hold keeps at most [`MAX_HOLD_BLOCKS`] blocks, so a
//!   run's pending nudges and holding blocks together never exceed what it
//!   keeps.
//!
//! # Two locks
//!
//! The book (memory) and the log (disk) have a lock each. Every change —
//! queueing, delivering, forgetting, compacting — takes the log lock first
//! and holds it across its I/O, so changes are serialized and the file and
//! memory never disagree; the book lock is only ever held for in-memory
//! work, never across a write or an fsync. A pre-call check with nothing to
//! deliver takes the book lock alone for one lookup, so no agent's check
//! waits behind another run's delivery or a compaction.
//!
//! An erasure of a run's session or trace, or of its tenant, removes its
//! nudges and rewrites the log without them. Whether an erasure covers a run
//! is asked while the log lock is held, which is what keeps a nudge queued
//! during an erasure from surviving it: the erasure installs its mask before
//! it forgets, and forgetting takes the same lock.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

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

/// What one tenant's nudges may cost together, in bytes (see
/// [`NUDGE_OVERHEAD_BYTES`]): 4 MiB.
pub const MAX_TENANT_NUDGE_BYTES: usize = 4 << 20;

/// What every tenant's nudges may cost together, in bytes: 32 MiB.
pub const MAX_HELD_NUDGE_BYTES: usize = 32 << 20;

/// What a nudge costs beyond its message, id, run id and tenant: its
/// timestamps, its place in the indexes and their copies of its keys.
pub const NUDGE_OVERHEAD_BYTES: usize = 512;

/// How long a delivered `block` keeps vetoing the run's pre-call checks, in
/// milliseconds: long enough to cover the parallel tool calls of the batch
/// it landed in, which check together; short enough that the agent's next
/// deliberate step — after a model call that reads the refusal — is judged
/// by the guard again.
pub const BLOCK_HOLD_MS: u64 = 2_000;

/// Most blocks one hold keeps vetoing with: the newest, those just
/// delivered first. A run's pending nudges and its holding blocks together
/// are never more than it keeps, so neither the per-run trim nor a byte
/// budget ever has to drop a veto in force.
pub const MAX_HOLD_BLOCKS: usize = MAX_PENDING_PER_RUN;

const _: () = assert!(MAX_PENDING_PER_RUN + MAX_HOLD_BLOCKS <= MAX_KEPT_PER_RUN);

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
    /// as the reason, and so does every check of the run for
    /// [`BLOCK_HOLD_MS`] after. It does not trip the fuse.
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

    /// What it counts against the budgets.
    fn cost(&self) -> usize {
        NUDGE_OVERHEAD_BYTES
            + self.message.len()
            + self.id.len()
            + self.run.len()
            + self.tenant.len()
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

/// The byte budgets a book enforces; [`Budget::STORE`] outside tests.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    /// Per tenant.
    pub(crate) tenant_bytes: usize,
    /// Across every tenant.
    pub(crate) total_bytes: usize,
}

impl Budget {
    /// The documented bounds.
    pub(crate) const STORE: Self = Self {
        tenant_bytes: MAX_TENANT_NUDGE_BYTES,
        total_bytes: MAX_HELD_NUDGE_BYTES,
    };
}

/// The blocks a delivery made, still vetoing the run's checks until
/// `until_ns`. Memory only: a restart forgets a hold of a few seconds.
#[derive(Clone, Debug)]
struct Hold {
    until_ns: u64,
    ids: Vec<String>,
}

/// One run's nudges, in the order they were queued, and the hold of its
/// latest blocking delivery.
#[derive(Clone, Debug, Default)]
struct RunNudges {
    nudges: Vec<Nudge>,
    hold: Option<Hold>,
}

impl RunNudges {
    fn pending(&self, now: u64) -> usize {
        self.nudges
            .iter()
            .filter(|nudge| nudge.status(now) == NudgeStatus::Pending)
            .count()
    }

    /// Whether `nudge` may be dropped to make room at `now`: it is settled
    /// — delivered or expired — and not a block of a hold still in force,
    /// which is a veto the run's checks are still answered with.
    fn droppable(&self, nudge: &Nudge, now: u64) -> bool {
        nudge.status(now) != NudgeStatus::Pending
            && !self
                .hold
                .as_ref()
                .is_some_and(|hold| now < hold.until_ns && hold.ids.contains(&nudge.id))
    }
}

/// Every tenant's nudges in memory, with what they cost and an index by
/// age for dropping the oldest. Never touched across I/O.
struct Book {
    runs: HashMap<String, HashMap<String, RunNudges>>,
    /// Every held nudge by `(created_at_ns, id)`: its tenant and run.
    order: BTreeMap<(u64, String), (String, String)>,
    /// What every held nudge costs together.
    held: usize,
    /// The same, per tenant; tenants holding nothing are absent.
    by_tenant: HashMap<String, usize>,
    budget: Budget,
}

/// A nudge to drop to make room: its tenant, run and id.
type Victim = (String, String, String);

impl Book {
    fn new(budget: Budget) -> Self {
        Self {
            runs: HashMap::new(),
            order: BTreeMap::new(),
            held: 0,
            by_tenant: HashMap::new(),
            budget,
        }
    }

    fn run(&self, tenant: &str, run: &str) -> Option<&RunNudges> {
        self.runs.get(tenant).and_then(|runs| runs.get(run))
    }

    /// Adds `nudge`, or replaces the one held under its id.
    fn insert(&mut self, nudge: Nudge) {
        let (tenant, run) = (nudge.tenant.clone(), nudge.run.clone());
        if let Some(index) = self
            .run(&tenant, &run)
            .and_then(|held| held.nudges.iter().position(|held| held.id == nudge.id))
        {
            self.remove_at(&tenant, &run, index);
        }
        let cost = nudge.cost();
        self.held += cost;
        *self.by_tenant.entry(tenant.clone()).or_default() += cost;
        self.order.insert(
            (nudge.created_at_ns, nudge.id.clone()),
            (tenant.clone(), run.clone()),
        );
        self.runs
            .entry(tenant)
            .or_default()
            .entry(run)
            .or_default()
            .nudges
            .push(nudge);
    }

    /// Removes the nudge at `index` of the run's list, and the run (and its
    /// tenant) when nothing is left.
    fn remove_at(&mut self, tenant: &str, run: &str, index: usize) -> Option<Nudge> {
        let runs = self.runs.get_mut(tenant)?;
        let held = runs.get_mut(run)?;
        if index >= held.nudges.len() {
            return None;
        }
        let nudge = held.nudges.remove(index);
        if held.nudges.is_empty() {
            runs.remove(run);
            if runs.is_empty() {
                self.runs.remove(tenant);
            }
        }
        self.order.remove(&(nudge.created_at_ns, nudge.id.clone()));
        let cost = nudge.cost();
        self.held = self.held.saturating_sub(cost);
        if let Some(spent) = self.by_tenant.get_mut(tenant) {
            *spent = spent.saturating_sub(cost);
            if *spent == 0 {
                self.by_tenant.remove(tenant);
            }
        }
        Some(nudge)
    }

    fn remove_id(&mut self, tenant: &str, run: &str, id: &str) {
        if let Some(index) = self
            .run(tenant, run)
            .and_then(|held| held.nudges.iter().position(|nudge| nudge.id == id))
        {
            self.remove_at(tenant, run, index);
        }
    }

    /// Drops the run's delivered and expired nudges, oldest first, beyond
    /// [`MAX_KEPT_PER_RUN`] — never a block of a hold still in force. That
    /// always suffices: a run holds at most [`MAX_PENDING_PER_RUN`] pending
    /// nudges and a hold at most [`MAX_HOLD_BLOCKS`] blocks.
    fn trim_run(&mut self, tenant: &str, run: &str, now: u64) {
        let Some(held) = self.run(tenant, run) else {
            return;
        };
        let excess = held.nudges.len().saturating_sub(MAX_KEPT_PER_RUN);
        let doomed: Vec<String> = held
            .nudges
            .iter()
            .filter(|nudge| held.droppable(nudge, now))
            .take(excess)
            .map(|nudge| nudge.id.clone())
            .collect();
        for id in doomed {
            self.remove_id(tenant, run, &id);
        }
    }

    /// The settled nudges to drop, oldest first, so that one costing `cost`
    /// fits `tenant`'s budget and the store's; a conflict, dropping
    /// nothing, when pending nudges and the blocks of holds still in force
    /// are all that would be left to drop.
    fn room_for(&self, tenant: &str, cost: usize, now: u64) -> Result<Vec<Victim>> {
        let spent = self.by_tenant.get(tenant).copied().unwrap_or(0);
        let tenant_need = (spent + cost).saturating_sub(self.budget.tenant_bytes);
        let total_need = (self.held + cost).saturating_sub(self.budget.total_bytes);
        let mut victims = Vec::new();
        let (mut tenant_freed, mut total_freed) = (0, 0);
        for ((_, id), (owner, run)) in &self.order {
            if tenant_freed >= tenant_need && total_freed >= total_need {
                break;
            }
            let own = owner == tenant;
            // Only the tenant's own budget is short: others' nudges cannot help.
            if !own && total_freed >= total_need {
                continue;
            }
            let Some((held, nudge)) = self.run(owner, run).and_then(|held| {
                held.nudges
                    .iter()
                    .find(|nudge| &nudge.id == id)
                    .map(|nudge| (held, nudge))
            }) else {
                continue;
            };
            if !held.droppable(nudge, now) {
                continue;
            }
            let freed = nudge.cost();
            total_freed += freed;
            if own {
                tenant_freed += freed;
            }
            victims.push((owner.clone(), run.clone(), id.clone()));
        }
        if tenant_freed < tenant_need {
            return Err(Error::Conflict(format!(
                "this tenant's nudges already cost the most Traza keeps for one tenant \
                 ({} bytes), and all that could make room are pending, or blocks whose veto \
                 still holds; wait for them to be delivered or to expire",
                self.budget.tenant_bytes
            )));
        }
        if total_freed < total_need {
            return Err(Error::Conflict(format!(
                "this server's nudges already cost the most it keeps ({} bytes), and all that \
                 could make room are pending, or blocks whose veto still holds; wait for them \
                 to be delivered or to expire",
                self.budget.total_bytes
            )));
        }
        Ok(victims)
    }

    /// Drops settled nudges, oldest first, until every budget holds (after
    /// a replay, whose log may hold what was dropped at run time). Pending
    /// nudges are kept even past a budget: they were accepted.
    fn fit_budgets(&mut self, now: u64) {
        let tenants: Vec<String> = self.by_tenant.keys().cloned().collect();
        for tenant in tenants {
            if let Ok(victims) = self.room_for(&tenant, 0, now) {
                for (owner, run, id) in victims {
                    self.remove_id(&owner, &run, &id);
                }
            }
        }
    }

    /// The ids of the run's pending nudges at `now`, in queue order.
    fn due(&self, tenant: &str, run: &str, now: u64) -> Vec<String> {
        self.run(tenant, run)
            .map(|held| {
                held.nudges
                    .iter()
                    .filter(|nudge| nudge.status(now) == NudgeStatus::Pending)
                    .map(|nudge| nudge.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The blocks of the run's hold at `now`, other than `except`.
    fn held_blocks(&self, tenant: &str, run: &str, now: u64, except: &[String]) -> Vec<Nudge> {
        let Some(held) = self.run(tenant, run) else {
            return Vec::new();
        };
        let Some(hold) = held.hold.as_ref().filter(|hold| now < hold.until_ns) else {
            return Vec::new();
        };
        held.nudges
            .iter()
            .filter(|nudge| hold.ids.contains(&nudge.id) && !except.contains(&nudge.id))
            .cloned()
            .collect()
    }

    /// Marks `ids` of the run delivered at `now` and returns them, followed
    /// by the blocks of a hold still in force; a block among them (re)starts
    /// the hold, with at most [`MAX_HOLD_BLOCKS`] blocks — those just
    /// delivered, then the newest carried.
    fn mark_delivered(&mut self, tenant: &str, run: &str, ids: &[String], now: u64) -> Vec<Nudge> {
        let mut delivered = Vec::with_capacity(ids.len());
        let Some(held) = self.runs.get_mut(tenant).and_then(|runs| runs.get_mut(run)) else {
            return delivered;
        };
        for nudge in &mut held.nudges {
            if ids.contains(&nudge.id) && nudge.delivered_at_ns.is_none() {
                nudge.delivered_at_ns = Some(now);
                delivered.push(nudge.clone());
            }
        }
        let fresh_ids: Vec<String> = delivered.iter().map(|nudge| nudge.id.clone()).collect();
        // A hold still in force keeps vetoing beside what was just delivered.
        let carried = self.held_blocks(tenant, run, now, &fresh_ids);
        if delivered.iter().any(|nudge| nudge.kind == NudgeKind::Block) {
            let blocks: Vec<String> = delivered
                .iter()
                .chain(&carried)
                .filter(|nudge| nudge.kind == NudgeKind::Block)
                .take(MAX_HOLD_BLOCKS)
                .map(|nudge| nudge.id.clone())
                .collect();
            if let Some(held) = self.runs.get_mut(tenant).and_then(|runs| runs.get_mut(run)) {
                held.hold = Some(Hold {
                    until_ns: now.saturating_add(BLOCK_HOLD_MS * 1_000_000),
                    ids: blocks,
                });
            }
        }
        delivered.extend(carried);
        delivered
    }

    /// The records that reproduce the state, for a rewrite.
    fn records(&self) -> Vec<Record> {
        self.order
            .iter()
            .filter_map(|((_, id), (tenant, run))| {
                self.run(tenant, run)
                    .and_then(|held| held.nudges.iter().find(|nudge| &nudge.id == id))
            })
            .map(|nudge| Record::Nudge {
                nudge: nudge.clone(),
            })
            .collect()
    }

    fn apply(&mut self, record: Record) {
        match record {
            Record::Nudge { nudge } => self.insert(nudge),
            Record::Deliver {
                tenant,
                run,
                ids,
                at_ns,
            } => {
                if let Some(held) = self
                    .runs
                    .get_mut(&tenant)
                    .and_then(|runs| runs.get_mut(&run))
                {
                    for nudge in &mut held.nudges {
                        if ids.contains(&nudge.id) && nudge.delivered_at_ns.is_none() {
                            nudge.delivered_at_ns = Some(at_ns);
                        }
                    }
                }
            }
        }
    }

    /// Removes one run's nudges, or a whole tenant's.
    fn forget(&mut self, tenant: &str, run: Option<&str>) {
        let runs: Vec<String> = match (run, self.runs.get(tenant)) {
            (Some(run), _) => vec![run.to_owned()],
            (None, Some(runs)) => runs.keys().cloned().collect(),
            (None, None) => Vec::new(),
        };
        for run in runs {
            while self.remove_at(tenant, &run, 0).is_some() {}
        }
    }
}

/// Every tenant's nudges and the log that makes them durable; see the
/// module docs for the locking.
pub(crate) struct Nudges {
    log: Mutex<FuseLog<Record>>,
    book: Mutex<Book>,
}

impl Nudges {
    /// Opens `steer.jsonl` in `directory` and replays it.
    pub(crate) fn open(directory: &Path, now: u64) -> Result<Self> {
        Self::open_with(directory, now, Budget::STORE)
    }

    /// [`Self::open`], with `budget` in place of the documented one.
    pub(crate) fn open_with(directory: &Path, now: u64, budget: Budget) -> Result<Self> {
        let (mut log, records) = FuseLog::<Record>::open_named(directory, LOG_NAME)?;
        let mut book = Book::new(budget);
        for record in records {
            book.apply(record);
        }
        let runs: Vec<(String, String)> = book
            .runs
            .iter()
            .flat_map(|(tenant, runs)| runs.keys().map(move |run| (tenant.clone(), run.clone())))
            .collect();
        for (tenant, run) in runs {
            book.trim_run(&tenant, &run, now);
        }
        book.fit_budgets(now);
        if log.compaction_due() {
            log.compact(&book.records())?;
        }
        Ok(Self {
            log: Mutex::new(log),
            book: Mutex::new(book),
        })
    }

    fn lock_log(&self) -> Result<MutexGuard<'_, FuseLog<Record>>> {
        self.log
            .lock()
            .map_err(|_| Error::LockPoisoned("nudge log"))
    }

    fn lock_book(&self) -> Result<MutexGuard<'_, Book>> {
        self.book.lock().map_err(|_| Error::LockPoisoned("nudges"))
    }

    /// After a durable change, with the log lock held: compacts the log when
    /// enough of it is superseded, the records staged under the book lock
    /// and written after it is released. Best effort: the change is already
    /// durable, and a failed compaction leaves a valid log that the next
    /// change compacts again — failing the request instead would lose a
    /// delivered nudge, or invite a duplicate.
    fn compact_if_due(&self, log: &mut FuseLog<Record>) {
        if !log.compaction_due() {
            return;
        }
        let Ok(book) = self.lock_book() else {
            return;
        };
        let records = book.records();
        drop(book);
        let _ = log.compact(&records);
    }

    /// Queues a nudge for `run`, durably. `covered` says whether a pending
    /// erasure covers the run; it is asked with the log lock held, so an
    /// erasure either sees this nudge and forgets it or is seen by it.
    pub(crate) fn create(
        &self,
        tenant: &str,
        run: &str,
        spec: &NudgeSpec,
        now: u64,
        covered: &dyn Fn() -> bool,
    ) -> Result<Nudge> {
        spec.validate()?;
        let mut log = self.lock_log()?;
        if covered() {
            return Err(Error::Conflict(format!(
                "an erasure covering run `{run}` is pending; nudge it once the erasure settles"
            )));
        }
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
        let victims = {
            let book = self.lock_book()?;
            let pending = book.run(tenant, run).map_or(0, |held| held.pending(now));
            if pending >= MAX_PENDING_PER_RUN {
                return Err(Error::Conflict(format!(
                    "run `{run}` already has {MAX_PENDING_PER_RUN} pending nudges; wait for its \
                     next check to deliver them, or for them to expire"
                )));
            }
            book.room_for(tenant, nudge.cost(), now)?
        };
        // Every change holds the log lock, so the book cannot move between
        // the plan above and its application below.
        log.append(&Record::Nudge {
            nudge: nudge.clone(),
        })?;
        {
            let mut book = self.lock_book()?;
            for (owner, held_run, id) in victims {
                book.remove_id(&owner, &held_run, &id);
            }
            book.insert(nudge.clone());
            book.trim_run(tenant, run, now);
        }
        self.compact_if_due(&mut log);
        Ok(nudge)
    }

    /// `run`'s nudges, in the order they were queued.
    pub(crate) fn list(&self, tenant: &str, run: &str) -> Result<Vec<Nudge>> {
        Ok(self
            .lock_book()?
            .run(tenant, run)
            .map(|held| held.nudges.clone())
            .unwrap_or_default())
    }

    /// Nudges pending at `now` for each `(tenant, run)`, under one
    /// acquisition of the book.
    pub(crate) fn pending_each<'a>(
        &self,
        runs: impl IntoIterator<Item = (&'a str, &'a str)>,
        now: u64,
    ) -> Result<Vec<usize>> {
        let book = self.lock_book()?;
        Ok(runs
            .into_iter()
            .map(|(tenant, run)| book.run(tenant, run).map_or(0, |held| held.pending(now)))
            .collect())
    }

    /// Delivers `run`'s pending nudges: marks them delivered, durably, and
    /// returns them in the order they were queued — followed by the blocks
    /// of a delivery in the last [`BLOCK_HOLD_MS`], which still veto. With
    /// nothing pending this takes the book lock for one lookup and writes
    /// nothing. If the delivery cannot be written, nothing is delivered and
    /// the nudges stay pending.
    pub(crate) fn deliver(&self, tenant: &str, run: &str, now: u64) -> Result<Vec<Nudge>> {
        {
            let book = self.lock_book()?;
            if book.due(tenant, run, now).is_empty() {
                return Ok(book.held_blocks(tenant, run, now, &[]));
            }
        }
        let mut log = self.lock_log()?;
        // Another check may have delivered them while this one waited.
        let due = {
            let book = self.lock_book()?;
            let due = book.due(tenant, run, now);
            if due.is_empty() {
                return Ok(book.held_blocks(tenant, run, now, &[]));
            }
            due
        };
        log.append(&Record::Deliver {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            ids: due.clone(),
            at_ns: now,
        })?;
        let delivered = self.lock_book()?.mark_delivered(tenant, run, &due, now);
        self.compact_if_due(&mut log);
        Ok(delivered)
    }

    /// Removes the nudges of one run (`Some(run)`) or of the whole tenant,
    /// and rewrites the log from what remains — always, since superseded
    /// lines can name the subject when memory no longer does.
    pub(crate) fn forget(&self, tenant: &str, run: Option<&str>) -> Result<()> {
        let mut log = self.lock_log()?;
        let records = {
            let mut book = self.lock_book()?;
            book.forget(tenant, run);
            book.records()
        };
        log.rewrite(&records)
    }

    /// What the held nudges cost: across every tenant, and `tenant`'s.
    #[cfg(test)]
    fn spent(&self, tenant: &str) -> (usize, usize) {
        let book = self.lock_book().expect("book");
        (book.held, book.by_tenant.get(tenant).copied().unwrap_or(0))
    }
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
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    const MS: u64 = 1_000_000;

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

    fn free() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn a_nudge_is_delivered_once_and_survives_a_restart_as_delivered() {
        let path = dir("once");
        let nudges = Nudges::open(&path, 1).expect("opens");
        let advice = nudges
            .create(
                "",
                "run-1",
                &spec(NudgeKind::Advice, "use the cache"),
                10,
                &free(),
            )
            .expect("queues");
        assert!(advice.id.starts_with("nd_") && advice.id.len() == 19);
        assert_eq!(
            advice.expires_at_ns,
            10 + DEFAULT_NUDGE_TTL_SECONDS * 1_000_000_000
        );
        assert_eq!(
            nudges.pending_each([("", "run-1")], 11).expect("counts"),
            [1]
        );
        let delivered = nudges.deliver("", "run-1", 20).expect("delivers");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].delivered_at_ns, Some(20));
        assert!(
            nudges.deliver("", "run-1", 21).expect("nothing").is_empty(),
            "a nudge reaches one check"
        );
        drop(nudges);
        let nudges = Nudges::open(&path, 30).expect("reopens");
        let listed = nudges.list("", "run-1").expect("lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status(30), NudgeStatus::Delivered);
        assert_eq!(listed[0].delivered_at_ns, Some(20));
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn a_block_vetoes_every_check_of_its_batch_and_then_lets_go() {
        let path = dir("hold");
        let nudges = Nudges::open(&path, 1).expect("opens");
        let block = nudges
            .create("", "r", &spec(NudgeKind::Block, "no refunds"), 10, &free())
            .expect("queues");
        let advice = nudges
            .create("", "r", &spec(NudgeKind::Advice, "be brief"), 11, &free())
            .expect("queues");
        let at = 1_000 * MS;
        let first = nudges.deliver("", "r", at).expect("delivers");
        let ids: Vec<&str> = first.iter().map(|nudge| nudge.id.as_str()).collect();
        assert_eq!(ids, [block.id.as_str(), advice.id.as_str()]);
        // The batch's other checks: the block again, never the advice.
        for later in [at, at + 1, at + BLOCK_HOLD_MS * MS - 1] {
            let held = nudges.deliver("", "r", later).expect("holds");
            assert_eq!(held.len(), 1, "at {later}: {held:?}");
            assert_eq!(held[0].id, block.id);
            assert_eq!(held[0].delivered_at_ns, Some(at), "delivered once");
        }
        assert!(
            nudges
                .deliver("", "r", at + BLOCK_HOLD_MS * MS)
                .expect("lets go")
                .is_empty(),
            "the next step after the hold is the guard's again"
        );
        // Advice alone holds nothing.
        nudges
            .create("", "q", &spec(NudgeKind::Advice, "x"), 10, &free())
            .expect("queues");
        assert_eq!(nudges.deliver("", "q", at).expect("delivers").len(), 1);
        assert!(nudges.deliver("", "q", at + 1).expect("none").is_empty());
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn expired_nudges_are_not_delivered() {
        let path = dir("expiry");
        let nudges = Nudges::open(&path, 1).expect("opens");
        let short = NudgeSpec {
            ttl_seconds: Some(1),
            ..spec(NudgeKind::Block, "stop")
        };
        nudges.create("", "r", &short, 0, &free()).expect("queues");
        let later = 2_000_000_000;
        assert!(nudges.deliver("", "r", later).expect("none").is_empty());
        assert_eq!(
            nudges.list("", "r").expect("lists")[0].status(later),
            NudgeStatus::Expired
        );
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn bounds_are_enforced_and_say_why() {
        let path = dir("bounds");
        let nudges = Nudges::open(&path, 1).expect("opens");
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
            match nudges.create("", "r", &bad, 1, &free()) {
                Err(Error::Invalid(message)) => assert!(message.contains(needle), "{message}"),
                other => panic!("expected {needle}, got {other:?}"),
            }
        }
        nudges
            .create(
                "",
                "r",
                &spec(NudgeKind::Advice, &"x".repeat(MAX_NUDGE_BYTES)),
                1,
                &free(),
            )
            .expect("exactly the cap fits");
        for _ in 1..MAX_PENDING_PER_RUN {
            nudges
                .create("", "r", &spec(NudgeKind::Advice, "x"), 1, &free())
                .expect("queues");
        }
        match nudges.create("", "r", &spec(NudgeKind::Advice, "x"), 1, &free()) {
            Err(Error::Conflict(message)) => assert!(message.contains("32 pending"), "{message}"),
            other => panic!("the 33rd pending nudge must be refused, got {other:?}"),
        }
        // Delivered ones make room, and history stays bounded.
        nudges.deliver("", "r", 2).expect("delivers");
        for round in 0..(MAX_KEPT_PER_RUN as u64 * 2) {
            let at = 10_000 * MS * (round + 1);
            nudges
                .create("", "r", &spec(NudgeKind::Advice, "x"), at, &free())
                .expect("queues");
            nudges.deliver("", "r", at + 1).expect("delivers");
        }
        assert!(nudges.list("", "r").expect("lists").len() <= MAX_KEPT_PER_RUN);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A nudge of `bytes` message bytes for run `run` costs this much.
    fn cost(tenant: &str, run: &str, bytes: usize) -> usize {
        NUDGE_OVERHEAD_BYTES + bytes + 19 + run.len() + tenant.len()
    }

    #[test]
    fn byte_budgets_drop_settled_history_first_and_refuse_only_pending() {
        let path = dir("budget");
        let each = cost("a", "r0", 100);
        let budget = Budget {
            tenant_bytes: each * 4,
            total_bytes: each * 6,
        };
        let nudges = Nudges::open_with(&path, 1, budget).expect("opens");
        let message = "m".repeat(100);
        let advice = spec(NudgeKind::Advice, &message);
        // Four pending nudges fill tenant a.
        for index in 0..4 {
            nudges
                .create("a", &format!("r{index}"), &advice, 10 + index, &free())
                .expect("fits");
        }
        assert_eq!(nudges.spent("a"), (each * 4, each * 4));
        match nudges.create("a", "r4", &advice, 20, &free()) {
            Err(Error::Conflict(message)) => {
                assert!(message.contains("for one tenant"), "{message}")
            }
            other => panic!("a fifth pending nudge must be refused, got {other:?}"),
        }
        // Delivered, they are history, and the oldest makes room.
        nudges.deliver("a", "r0", 30).expect("delivers");
        nudges
            .create("a", "r4", &advice, 40, &free())
            .expect("the oldest settled nudge made room");
        assert!(nudges.list("a", "r0").expect("lists").is_empty());
        assert_eq!(nudges.list("a", "r1").expect("lists").len(), 1);
        assert_eq!(nudges.spent("a").1, each * 4);
        // Tenant b fits its own budget until the store's runs out: two more.
        for index in 0..2 {
            nudges
                .create("b", &format!("r{index}"), &advice, 50 + index, &free())
                .expect("fits the store");
        }
        match nudges.create("b", "r2", &advice, 60, &free()) {
            Err(Error::Conflict(message)) => assert!(message.contains("this server"), "{message}"),
            other => panic!("the store's budget must refuse, got {other:?}"),
        }
        // Another tenant's settled history makes room for the store's.
        nudges.deliver("a", "r1", 70).expect("delivers");
        nudges
            .create("b", "r2", &advice, 80, &free())
            .expect("a's delivered nudge made room");
        assert!(nudges.list("a", "r1").expect("lists").is_empty());
        let (total, _) = nudges.spent("b");
        assert!(total <= budget.total_bytes, "{total}");
        // A replay of the log, which still holds what was dropped, fits too.
        drop(nudges);
        let nudges = Nudges::open_with(&path, 90, budget).expect("reopens");
        let (total, a) = nudges.spent("a");
        assert!(total <= budget.total_bytes && a <= budget.tenant_bytes);
        assert!(nudges.list("a", "r0").expect("lists").is_empty());
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A delivered block still holding is a veto in force: a byte budget's
    /// eviction may not take it to make room, or the next check of its
    /// batch goes through. Once its hold is over it is history like any
    /// other.
    #[test]
    fn a_holding_block_is_never_evicted_to_make_room() {
        let path = dir("hold-budget");
        let each = cost("a", "r0", 100);
        let budget = Budget {
            tenant_bytes: each * 2,
            total_bytes: each * 8,
        };
        let nudges = Nudges::open_with(&path, 1, budget).expect("opens");
        let message = "m".repeat(100);
        let block = nudges
            .create("a", "r0", &spec(NudgeKind::Block, &message), 10, &free())
            .expect("queues");
        let advice = spec(NudgeKind::Advice, &message);
        nudges
            .create("a", "r1", &advice, 11, &free())
            .expect("fits");
        let at = 1_000 * MS;
        assert_eq!(
            nudges.deliver("a", "r0", at).expect("delivers")[0].id,
            block.id
        );
        // The tenant is full, and its one settled nudge is the block, still
        // holding: nothing may make room.
        match nudges.create("a", "r2", &advice, at + 1, &free()) {
            Err(Error::Conflict(message)) => {
                assert!(message.contains("veto"), "{message}")
            }
            other => panic!("the holding block must not be dropped, got {other:?}"),
        }
        let held = nudges.deliver("a", "r0", at + 2).expect("holds");
        let ids: Vec<&str> = held.iter().map(|nudge| nudge.id.as_str()).collect();
        assert_eq!(ids, [block.id.as_str()], "the batch's next check is vetoed");
        // Its hold over, the block is history and makes room.
        nudges
            .create("a", "r2", &advice, at + BLOCK_HOLD_MS * MS, &free())
            .expect("the block's hold is over");
        assert!(nudges.list("a", "r0").expect("lists").is_empty());
        let _ = std::fs::remove_dir_all(&path);
    }

    /// The per-run trim spares a holding block too, and the hold's own
    /// size is bounded, so a run still keeps at most [`MAX_KEPT_PER_RUN`].
    #[test]
    fn a_holding_block_outlives_the_per_run_trim_and_holds_stay_bounded() {
        let path = dir("hold-trim");
        let nudges = Nudges::open(&path, 1).expect("opens");
        let block = nudges
            .create("", "r", &spec(NudgeKind::Block, "halt"), 10, &free())
            .expect("queues");
        let at = 1_000 * MS;
        nudges.deliver("", "r", at).expect("delivers");
        // Within the hold, the run's history fills past what it keeps.
        for index in 0..MAX_KEPT_PER_RUN as u64 {
            let now = at + 1 + index * 2;
            nudges
                .create("", "r", &spec(NudgeKind::Advice, "x"), now, &free())
                .expect("queues");
            let delivered = nudges.deliver("", "r", now + 1).expect("delivers");
            assert!(
                delivered.iter().any(|nudge| nudge.id == block.id),
                "check {index} is still vetoed: {delivered:?}"
            );
            assert!(nudges.list("", "r").expect("lists").len() <= MAX_KEPT_PER_RUN);
        }
        // Blocks delivered while a hold is in force join it — up to the
        // hold's bound, the newest kept — and the run stays inside its own.
        let later = at + 10 * BLOCK_HOLD_MS * MS;
        for round in 0..3_u64 {
            let now = later + round * 1_000;
            for index in 0..MAX_PENDING_PER_RUN as u64 {
                nudges
                    .create("", "r", &spec(NudgeKind::Block, "no"), now + index, &free())
                    .expect("queues");
            }
            let delivered = nudges.deliver("", "r", now + 500).expect("delivers");
            assert!(delivered.len() <= MAX_PENDING_PER_RUN + MAX_HOLD_BLOCKS);
            assert!(nudges.list("", "r").expect("lists").len() <= MAX_KEPT_PER_RUN);
            let held = nudges.deliver("", "r", now + 501).expect("holds");
            assert_eq!(held.len(), MAX_HOLD_BLOCKS, "round {round}");
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn random_changes_keep_the_accounting_exact() {
        let path = dir("accounting");
        let budget = Budget {
            tenant_bytes: cost("t0", "r0", 40) * 6,
            total_bytes: cost("t0", "r0", 40) * 10,
        };
        let nudges = Nudges::open_with(&path, 1, budget).expect("opens");
        let mut rng = crate::seed::Rng::new(0x0ddb_0d6e);
        let mut now = 10;
        for _ in 0..250 {
            now += rng.range(1, 3) * 1_000 * MS;
            let tenant = format!("t{}", rng.range(0, 2));
            let run = format!("r{}", rng.range(0, 3));
            match rng.range(0, 9) {
                0..=4 => {
                    let message = "z".repeat(rng.range(1, 40) as usize);
                    let kind = if rng.chance(30) {
                        NudgeKind::Block
                    } else {
                        NudgeKind::Advice
                    };
                    let ttl = Some(rng.range(1, 4));
                    let _ = nudges.create(
                        &tenant,
                        &run,
                        &NudgeSpec {
                            message,
                            kind,
                            ttl_seconds: ttl,
                        },
                        now,
                        &free(),
                    );
                }
                5..=7 => {
                    nudges.deliver(&tenant, &run, now).expect("delivers");
                }
                8 => nudges.forget(&tenant, Some(&run)).expect("forgets"),
                _ => nudges.forget(&tenant, None).expect("forgets"),
            }
            let book = nudges.lock_book().expect("book");
            let mut total = 0;
            let mut by_tenant: HashMap<String, usize> = HashMap::new();
            let mut count = 0;
            for (tenant, runs) in &book.runs {
                for held in runs.values() {
                    assert!(!held.nudges.is_empty(), "no empty runs are kept");
                    assert!(held.nudges.len() <= MAX_KEPT_PER_RUN);
                    for nudge in &held.nudges {
                        total += nudge.cost();
                        *by_tenant.entry(tenant.clone()).or_default() += nudge.cost();
                        count += 1;
                    }
                }
            }
            assert_eq!(book.held, total);
            assert_eq!(book.by_tenant, by_tenant);
            assert_eq!(book.order.len(), count);
            assert!(book.held <= budget.total_bytes);
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn forgetting_rewrites_the_log_without_the_subject() {
        let path = dir("forget");
        let nudges = Nudges::open(&path, 1).expect("opens");
        nudges
            .create(
                "acme",
                "secret-session",
                &spec(NudgeKind::Advice, "a"),
                1,
                &free(),
            )
            .expect("queues");
        nudges
            .create("acme", "kept", &spec(NudgeKind::Advice, "b"), 1, &free())
            .expect("queues");
        nudges
            .create("other", "x", &spec(NudgeKind::Advice, "c"), 1, &free())
            .expect("queues");
        nudges
            .forget("acme", Some("secret-session"))
            .expect("forgets");
        let text = std::fs::read_to_string(path.join(LOG_NAME)).expect("reads");
        assert!(!text.contains("secret-session"), "{text}");
        assert!(text.contains("kept"));
        nudges.forget("acme", None).expect("forgets the tenant");
        let text = std::fs::read_to_string(path.join(LOG_NAME)).expect("reads");
        assert!(!text.contains("\"acme\""), "{text}");
        assert_eq!(nudges.list("other", "x").expect("lists").len(), 1);
        assert_eq!(nudges.spent("acme").1, 0);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// An erasure installing its mask and forgetting the run right after a
    /// nudge's check of the mask found nothing: the forget must still take
    /// the nudge, because it cannot run until the nudge is written.
    #[test]
    fn a_nudge_racing_an_erasure_does_not_survive_it() {
        let path = dir("race");
        let nudges = Arc::new(Nudges::open(&path, 1).expect("opens"));
        let masked = Arc::new(AtomicBool::new(false));
        let eraser: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> =
            std::sync::Mutex::new(None);
        let covered = || {
            let seen = masked.load(Ordering::SeqCst);
            // The erasure lands now: mask, then forget, on its own thread.
            let (nudges, masked) = (Arc::clone(&nudges), Arc::clone(&masked));
            let handle = std::thread::spawn(move || {
                masked.store(true, Ordering::SeqCst);
                nudges.forget("acme", Some("doomed")).expect("forgets");
            });
            // Long enough for the forget to finish if nothing holds it back.
            std::thread::sleep(std::time::Duration::from_millis(150));
            *eraser.lock().expect("eraser") = Some(handle);
            seen
        };
        nudges
            .create(
                "acme",
                "doomed",
                &spec(NudgeKind::Advice, "secret"),
                10,
                &covered,
            )
            .expect("queued before the erasure saw it");
        let handle = eraser.lock().expect("eraser").take().expect("erasure ran");
        handle.join().expect("the erasure finishes");
        assert!(
            nudges.list("acme", "doomed").expect("lists").is_empty(),
            "the erasure took the nudge from memory"
        );
        let text = std::fs::read_to_string(path.join(LOG_NAME)).expect("reads");
        assert!(!text.contains("secret"), "and from disk: {text}");
        // With the mask in place, a new nudge is refused.
        match nudges.create("acme", "doomed", &spec(NudgeKind::Advice, "x"), 20, &|| {
            masked.load(Ordering::SeqCst)
        }) {
            Err(Error::Conflict(message)) => assert!(message.contains("erasure"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A check with nothing to deliver never waits on the log: it answers
    /// while another thread holds the log lock (a delivery's fsync, a
    /// compaction).
    #[test]
    fn a_check_with_nothing_pending_does_not_wait_on_the_log() {
        let path = dir("fast");
        let nudges = Arc::new(Nudges::open(&path, 1).expect("opens"));
        let log = nudges.lock_log().expect("log");
        let (sender, receiver) = std::sync::mpsc::channel();
        let checker = Arc::clone(&nudges);
        let handle = std::thread::spawn(move || {
            let answer = checker.deliver("", "idle", 5).expect("answers");
            sender.send(answer.len()).expect("sends");
        });
        let answered = receiver.recv_timeout(std::time::Duration::from_secs(5));
        drop(log);
        handle.join().expect("joins");
        assert_eq!(
            answered,
            Ok(0),
            "the idle check answered under the log lock"
        );
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn a_torn_final_line_is_healed_at_open() {
        let path = dir("torn");
        let nudges = Nudges::open(&path, 1).expect("opens");
        nudges
            .create("", "r", &spec(NudgeKind::Advice, "whole"), 1, &free())
            .expect("queues");
        drop(nudges);
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path.join(LOG_NAME))
            .expect("opens the log");
        file.write_all(b"{\"op\":\"nudge\",\"nud").expect("tears");
        drop(file);
        let nudges = Nudges::open(&path, 2).expect("heals the torn tail");
        assert_eq!(nudges.list("", "r").expect("lists").len(), 1);
        let _ = std::fs::remove_dir_all(&path);
    }
}
