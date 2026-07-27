//! The live tail: a bounded ring of recent admissions.
//!
//! A tail answers "show me spans as they land", and that question is about
//! ADMISSION order — the order the store accepted spans in. Every other query
//! surface in the engine is ordered by EVENT time, and the two are not the
//! same: a span that ran for a minute starts before, and arrives after, spans
//! that started later. Paginating a tail by `start_time_ns` therefore drops
//! exactly the spans an observability tool exists to show. The watermark moves
//! past a long operation while it is still running, and when it finally lands
//! the server filters it out permanently. It is not lag; it is a silent,
//! unrecoverable hole, and no amount of client-side cleverness closes it
//! because the ordering the tail needs is not in the data being paged.
//!
//! So admission order is materialized here instead, and ONLY here. The
//! alternative was an `ingest_seq` field persisted on every span record plus a
//! seq range in every segment header, which buys durable admission order over
//! the whole corpus. That was rejected deliberately:
//!
//! - A tail never wants admission order over cold segments. Scrolled back far
//!   enough, you are reading history, and history is properly event-time.
//! - It puts a field on the innermost scan loop — a branch in `span_matches`
//!   for every query, including the overwhelming majority that never filter on
//!   it — to serve one screen.
//! - Compaction merges segments, so a persisted seq range widens toward the
//!   union of its inputs. The pruning it was supposed to buy decays exactly as
//!   the store grows, which is when it would have mattered.
//!
//! What that design buys and this one does not is durable, unbounded replay:
//! "everything admitted since I last synced, across restarts". That is export
//! or change-data-capture, not a tail, and if it is ever built it wants the
//! persisted field. Until then this costs no disk, no format version and no
//! branch in the query path.
//!
//! **"Admitted" means acknowledged.** A span enters the ring only after its
//! ingest has succeeded — after the write-ahead log's fsync, and after the
//! synchronous seal that `Durability::Flushed` promises. A live view is allowed
//! to be bounded and to admit gaps; it is not allowed to show data the store
//! never accepted. Publishing at the write-buffer upsert instead, which is
//! where this began, let the tail show spans whose ingest then returned an
//! error. Sequence numbers are therefore assigned in acknowledgement order,
//! which is what a caller observed and what a replicated commit position would
//! later refine into a total order.
//!
//! The ring holds `Arc<Span>` — the same handles [`crate::WriteBuffer`] holds —
//! so a span in both costs one pointer here, not a copy. Entries outlive the
//! buffer's eviction, which is the point: a span sealed into a segment two
//! seconds ago is still what a tail wants to show.

use crate::Span;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A subscriber's position in the admission stream.
///
/// `seq` is the next sequence number the subscriber wants, not the last one it
/// saw. That choice removes an off-by-one at both ends: a fresh subscriber
/// starts at the ring's oldest retained seq with no "minus one" that underflows
/// at zero, and a subscriber that consumed `k` entries from index `i` resumes
/// at `i + k` with no adjustment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailCursor {
    /// Identifies the process that issued the cursor.
    ///
    /// Sequence numbers live in memory and restart at zero, so without this a
    /// cursor from before a restart would be silently misread as a valid
    /// position in the new process's numbering and skip everything below it.
    /// A mismatch is reported as a gap, which the client can actually recover
    /// from.
    pub epoch: u64,
    /// The next sequence number wanted.
    pub seq: u64,
}

impl TailCursor {
    /// Renders the cursor as `epoch.seq`.
    ///
    /// Deliberately readable rather than opaque. A tail cursor is a position in
    /// a volatile in-memory ring, not a promise about storage layout, so there
    /// is nothing here to keep clients from depending on — and being able to
    /// read one out of a log is worth more than the encoding discipline.
    pub fn to_token(&self) -> String {
        format!("{}.{}", self.epoch, self.seq)
    }

    /// Parses a token produced by [`Self::to_token`].
    pub fn parse(token: &str) -> Option<Self> {
        let (epoch, seq) = token.split_once('.')?;
        Some(Self {
            epoch: epoch.parse().ok()?,
            seq: seq.parse().ok()?,
        })
    }
}

/// The outcome of reading from a subscriber's position.
#[derive(Debug)]
pub enum TailRead {
    /// Spans matching the subscriber's filter, and the position to resume from.
    ///
    /// The span list may be empty while the cursor still advances: entries were
    /// admitted but none passed the filter. Advancing anyway is what keeps a
    /// heavily filtered subscriber from falling off the back of the ring and
    /// gapping on traffic it never wanted.
    Batch {
        /// Matching spans, in admission order.
        spans: Vec<Arc<Span>>,
        /// Where to resume.
        cursor: TailCursor,
    },
    /// The position is no longer in the ring, and what was missed cannot be
    /// reconstructed.
    ///
    /// **This carries no resumable position, deliberately.** An earlier design
    /// returned the ring's floor so a client could "backfill only what was
    /// dropped", and that was incoherent: the dropped entries are precisely the
    /// ones no longer addressable, and the only other query surface is ordered
    /// by event time, which cannot name an admission range at all. What it
    /// actually produced was a fetch overlapping the entries the stream then
    /// replayed, and duplicates with nothing to deduplicate them.
    ///
    /// A gap is a discontinuity. The subscriber discards what it has and the
    /// stream resumes with a fresh backlog from the live edge — one ordered
    /// source, no overlap, and no claim of completeness across the break.
    Gap {
        /// Admissions lost, or `None` when the position came from another
        /// process and the count is therefore not comparable.
        missed: Option<u64>,
    },
}

/// One retained admission and what it costs to retain.
///
/// The size is measured once, at push, and carried: recomputing it at eviction
/// would walk a span's whole attribute map again, and measuring it against the
/// span as it is now would drift if anything ever mutated in place.
struct Entry {
    span: Arc<Span>,
    bytes: usize,
}

/// A bounded ring of recently admitted spans.
///
/// Bounded two ways, and it needs both. A count alone says nothing about
/// memory: a span carrying a 64 KiB prompt is three orders of magnitude larger
/// than one carrying a status code, and the ring is the sole owner of that
/// allocation once a seal has evicted it from the write buffer. Counting only
/// entries, 8,192 of them reached hundreds of megabytes of LLM text — exactly
/// the residency the attribute index was rewritten to remove.
pub struct TailRing {
    epoch: u64,
    /// The sequence number of `entries.front()`; when empty, the number the
    /// next push will take.
    ///
    /// Sequence numbers are contiguous, so storing one per entry would be eight
    /// bytes of redundancy that can disagree with the ring's actual contents.
    /// The seq of `entries[i]` is `first_seq + i`, by construction.
    first_seq: u64,
    entries: VecDeque<Entry>,
    capacity: usize,
    byte_budget: usize,
    resident_bytes: usize,
}

impl TailRing {
    /// A ring retaining at most `capacity` spans and `byte_budget` bytes of
    /// them, whichever binds first.
    pub fn new(capacity: usize, byte_budget: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            epoch: process_epoch(),
            first_seq: 0,
            entries: VecDeque::with_capacity(capacity.min(4_096)),
            capacity,
            byte_budget: byte_budget.max(1),
            resident_bytes: 0,
        }
    }

    /// Spans currently retained, and the bytes they hold.
    pub fn usage(&self) -> (usize, usize) {
        (self.entries.len(), self.resident_bytes)
    }

    /// The configured bounds, as `(spans, bytes)`.
    pub fn limits(&self) -> (usize, usize) {
        (self.capacity, self.byte_budget)
    }

    /// The sequence number the next admission will take.
    fn next_seq(&self) -> u64 {
        self.first_seq.saturating_add(self.entries.len() as u64)
    }

    /// The newest position — where a subscriber wanting only future spans
    /// starts.
    pub fn head(&self) -> TailCursor {
        TailCursor {
            epoch: self.epoch,
            seq: self.next_seq(),
        }
    }

    /// The oldest position still retained.
    pub fn floor(&self) -> TailCursor {
        TailCursor {
            epoch: self.epoch,
            seq: self.first_seq,
        }
    }

    /// Appends one admitted span, evicting the oldest until both bounds hold.
    ///
    /// The newest entry is never evicted, even when it alone exceeds the byte
    /// budget. A tail that showed nothing because one span was large would be
    /// a worse failure than briefly exceeding the bound, and the excess is
    /// bounded by one span either way.
    fn push(&mut self, span: Arc<Span>) {
        let bytes = approximate_bytes(&span);
        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.entries.push_back(Entry { span, bytes });
        while self.entries.len() > 1
            && (self.entries.len() > self.capacity || self.resident_bytes > self.byte_budget)
        {
            if let Some(evicted) = self.entries.pop_front() {
                self.resident_bytes = self.resident_bytes.saturating_sub(evicted.bytes);
            }
            self.first_seq = self.first_seq.saturating_add(1);
        }
    }

    /// Reads at most `limit` matching spans from `cursor`.
    ///
    /// `cursor` of `None` starts `backfill` entries behind the head, so a tail
    /// opening on a quiet store shows recent history instead of a blank screen
    /// until something happens to arrive. That backlog is free — it is already
    /// in memory.
    fn read(
        &self,
        cursor: Option<TailCursor>,
        backfill: usize,
        limit: usize,
        keep: &dyn Fn(&Span) -> bool,
    ) -> TailRead {
        let start = match cursor {
            None => self.entries.len().saturating_sub(backfill),
            Some(cursor) => {
                // Three ways a position can be unusable, all reported as a gap
                // because the honest answer to each is "resynchronize":
                // another process issued it; the entries it points at have
                // been evicted; or it claims to have seen more than was ever
                // emitted, which means it is corrupt.
                if cursor.epoch != self.epoch
                    || cursor.seq < self.first_seq
                    || cursor.seq > self.next_seq()
                {
                    return TailRead::Gap {
                        // How many admissions were lost, when that is knowable.
                        // A cursor from another process indexes a different
                        // numbering, so subtracting from it would produce a
                        // confident-looking number that means nothing.
                        missed: (cursor.epoch == self.epoch)
                            .then(|| self.first_seq.saturating_sub(cursor.seq)),
                    };
                }
                (cursor.seq - self.first_seq) as usize
            }
        };

        let mut spans = Vec::new();
        let mut consumed = 0_usize;
        for entry in self.entries.iter().skip(start) {
            if spans.len() == limit {
                break;
            }
            consumed += 1;
            if keep(&entry.span) {
                spans.push(Arc::clone(&entry.span));
            }
        }
        TailRead::Batch {
            spans,
            cursor: TailCursor {
                epoch: self.epoch,
                seq: self.first_seq.saturating_add((start + consumed) as u64),
            },
        }
    }
}

/// The ring plus the signal that makes a tail a stream rather than a poll.
///
/// Without the condvar a subscriber has to ask repeatedly whether anything has
/// arrived, which is the polling design this replaces — just moved inside the
/// process. With it, an idle tail costs one parked thread and nothing else, and
/// a span reaches a connected client as soon as the ingest that admitted it
/// releases the writer lock.
pub struct TailChannel {
    ring: Mutex<TailRing>,
    signal: Condvar,
}

impl TailChannel {
    /// A channel over a ring retaining at most `capacity` spans and
    /// `byte_budget` bytes of them.
    pub fn new(capacity: usize, byte_budget: usize) -> Self {
        Self {
            ring: Mutex::new(TailRing::new(capacity, byte_budget)),
            signal: Condvar::new(),
        }
    }

    /// Retained spans, retained bytes, and the two bounds — so an operator can
    /// see which one is binding rather than inferring it.
    pub fn usage(&self) -> Option<(usize, usize, usize, usize)> {
        let ring = self.ring.lock().ok()?;
        let (spans, bytes) = ring.usage();
        let (capacity, budget) = ring.limits();
        Some((spans, bytes, capacity, budget))
    }

    /// Records admitted spans and wakes every waiting subscriber.
    ///
    /// A poisoned ring is dropped rather than propagated: losing tail history
    /// degrades one screen, while failing an ingest that the store has already
    /// accepted would turn a cosmetic problem into data loss.
    pub fn publish(&self, spans: &[Arc<Span>]) {
        if spans.is_empty() {
            return;
        }
        if let Ok(mut ring) = self.ring.lock() {
            for span in spans {
                ring.push(Arc::clone(span));
            }
        }
        self.signal.notify_all();
    }

    /// The position a subscriber wanting only future spans starts from.
    pub fn head(&self) -> Option<TailCursor> {
        self.ring.lock().ok().map(|ring| ring.head())
    }

    /// Waits up to `timeout` for spans after `cursor`.
    ///
    /// Returns as soon as any entries are CONSUMED, even when the filter
    /// rejected all of them, so the returned cursor keeps pace with the ring.
    /// A subscriber whose filter matches nothing would otherwise sit at a fixed
    /// position while the ring turned over beneath it, and gap on traffic it
    /// had no interest in.
    ///
    /// On timeout the batch is empty and the cursor is unchanged: that is the
    /// heartbeat, and it is what distinguishes a quiet store from a dead
    /// connection.
    pub fn wait(
        &self,
        cursor: Option<TailCursor>,
        backfill: usize,
        limit: usize,
        timeout: Duration,
        keep: &dyn Fn(&Span) -> bool,
    ) -> TailRead {
        let unchanged = || TailRead::Batch {
            spans: Vec::new(),
            cursor: cursor.unwrap_or(TailCursor { epoch: 0, seq: 0 }),
        };
        let ring = match self.ring.lock() {
            Ok(ring) => ring,
            Err(_) => return unchanged(),
        };
        // Read before waiting: the spans the subscriber asked for may already
        // be here, and a condvar only reports what happens AFTER the wait
        // begins. Waiting first would delay every resumption by one timeout.
        let resume = match ring.read(cursor, backfill, limit, keep) {
            gap @ TailRead::Gap { .. } => return gap,
            found @ TailRead::Batch { .. } if advanced(&found, cursor) => return found,
            TailRead::Batch { cursor, .. } => cursor,
        };
        let (ring, _) = match self.signal.wait_timeout(ring, timeout) {
            Ok(pair) => pair,
            Err(_) => return unchanged(),
        };
        // `backfill` is deliberately not reapplied: the position is live now,
        // so re-deriving a start from the head would resend history the
        // subscriber has already been given.
        ring.read(Some(resume), 0, limit, keep)
    }
}

/// Whether a read moved the subscriber's position.
fn advanced(read: &TailRead, from: Option<TailCursor>) -> bool {
    match (read, from) {
        (TailRead::Batch { cursor, .. }, Some(from)) => cursor.seq != from.seq,
        // A subscriber with no cursor is opening the stream. Its first read is
        // the backlog, and it is delivered even when empty so the client learns
        // its starting position without waiting out a heartbeat.
        (TailRead::Batch { .. }, None) => true,
        (TailRead::Gap { .. }, _) => true,
    }
}

/// Roughly what retaining `span` costs, in bytes.
///
/// Public so `tests/tail_memory.rs` can check it against a counting allocator
/// — the only check that covers shapes nobody thought to write a case for.
///
/// Approximate on purpose. It counts the heap a span's own strings and JSON
/// values hold and ignores per-allocation overhead, because the number is a
/// budget input rather than an accounting figure — it has to track the thing
/// that actually varies by orders of magnitude, which is text.
///
/// Measured once per admitted span, and cheaper than what admit already does
/// to the same span: the write-ahead log serializes it in full.
pub fn approximate_bytes(span: &Span) -> usize {
    const OVERHEAD: usize = std::mem::size_of::<Span>();
    /// Charged per map entry and per collection element, matching `value_bytes`.
    const ENTRY: usize = 48 + std::mem::size_of::<String>() + std::mem::size_of::<Value>();

    // Destructured exhaustively, with no `..`, ON PURPOSE.
    //
    // Every hole found in this function so far was a place the walk did not
    // reach: text-only counting missed structure, then structure-counting
    // missed retained capacity. The remaining risk of the same kind is a field
    // added to `Span` later and not added here — a silent undercount that no
    // test would notice, because a test can only probe shapes someone thought
    // of. This makes it a compile error instead.
    //
    // Serializing the span and measuring that would be exact for what goes on
    // the wire, and wrong for what goes in the ring: a `Vec` with 100,000
    // elements of spare capacity serializes to a few bytes while holding
    // megabytes. The ring holds the allocation, so the allocation is what has
    // to be counted.
    let Span {
        trace_id,
        span_id,
        parent_span_id,
        name,
        start_time_ns: _,
        end_time_ns: _,
        status,
        service,
        attributes,
        events,
        links,
        extra,
    } = span;

    let mut total = OVERHEAD
        + trace_id.capacity()
        + span_id.capacity()
        + name.capacity()
        + status.capacity()
        + service.capacity()
        + parent_span_id.as_ref().map_or(0, String::capacity);
    for (key, value) in attributes {
        total += ENTRY + key.capacity() + value_bytes(value);
    }
    for (key, value) in extra {
        total += ENTRY + key.capacity() + value_bytes(value);
    }
    // Both vectors are charged by capacity for the same reason as an array
    // below: a truncated `Vec` keeps its allocation, and the ring keeps the
    // `Vec`.
    total += events.capacity() * std::mem::size_of::<crate::Event>();
    for event in events {
        let crate::Event {
            name,
            timestamp_ns: _,
            attributes,
        } = event;
        total += name.capacity();
        for (key, value) in attributes {
            total += ENTRY + key.capacity() + value_bytes(value);
        }
    }
    total += links.capacity() * std::mem::size_of::<crate::Link>();
    for link in links {
        let crate::Link {
            trace_id,
            span_id,
            attributes,
        } = link;
        total += trace_id.capacity() + span_id.capacity();
        for (key, value) in attributes {
            total += ENTRY + key.capacity() + value_bytes(value);
        }
    }
    total
}

/// Heap held by one JSON value, recursively.
fn value_bytes(value: &Value) -> usize {
    // Every element of an array and every entry of a map costs a whole `Value`
    // slot regardless of what it holds, plus the container's own bookkeeping.
    //
    // The first version counted only text: scalars were free and a container
    // cost eight bytes per element. That made the byte budget bypassable by
    // shape rather than by size — deeply nested structured JSON with no long
    // strings in it measured at a small fraction of what it actually held, so
    // 128 spans could take 150 MB against a 32 MiB ceiling the accounting
    // believed was barely touched. Anything the ring holds has to be counted,
    // and where the exact cost is an allocator detail the estimate rounds up.
    const SLOT: usize = std::mem::size_of::<Value>();
    const KEY: usize = std::mem::size_of::<String>();
    /// Per-entry overhead of the map `serde_json` builds. A rounded-up stand-in
    /// for node and hashing structure that no public API exposes.
    const ENTRY: usize = 48;

    match value {
        Value::String(text) => KEY + text.capacity(),
        // CAPACITY, not length. A `Vec` grown to 100,000 elements and then
        // truncated to one still owns the whole allocation, and a library
        // caller can hand exactly that to `Store::ingest` — 32 such spans held
        // 100 MB while the accounting reported 13 KB.
        Value::Array(items) => {
            SLOT * items.capacity() + items.iter().map(value_bytes).sum::<usize>()
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(key, nested)| ENTRY + KEY + key.capacity() + SLOT + value_bytes(nested))
            .sum(),
        // Null, bool and number live inline in the enum, which the caller has
        // already charged for as a slot.
        _ => 0,
    }
}

/// An identifier for this process's sequence numbering.
///
/// Wall-clock nanoseconds at construction. Two runs of the same store would
/// have to start within the same nanosecond to collide, and the consequence of
/// a collision is one client resuming from a stale position rather than
/// gapping — the same failure the tail already tolerates when the ring turns
/// over.
fn process_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, start_ns: u64) -> Arc<Span> {
        Arc::new(Span {
            trace_id: format!("t-{id}"),
            span_id: id.to_string(),
            parent_span_id: None,
            name: "op".into(),
            start_time_ns: start_ns,
            end_time_ns: start_ns + 1,
            status: "ok".into(),
            service: "svc".into(),
            attributes: Default::default(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Default::default(),
        })
    }

    fn span_with_id(id: &str) -> Arc<Span> {
        span(id, 1)
    }

    fn all(_: &Span) -> bool {
        true
    }

    #[test]
    fn delivers_a_span_that_started_before_the_one_already_seen() {
        // The bug this module exists for. Under event-time paging, admitting a
        // span that STARTED earlier than one already delivered leaves it
        // permanently invisible: it sorts below the watermark the client has
        // moved past. Admission order has no such hole.
        let ring = TailChannel::new(16, usize::MAX);
        ring.publish(&[span("a", 10_000)]);
        let first = ring.wait(None, 100, 100, Duration::ZERO, &all);
        let cursor = match first {
            TailRead::Batch { spans, cursor } => {
                assert_eq!(spans.len(), 1);
                assert_eq!(spans[0].span_id, "a");
                cursor
            }
            TailRead::Gap { .. } => panic!("a fresh subscriber cannot gap"),
        };

        // Starts BEFORE `a`, lands after it.
        ring.publish(&[span("b", 5_000)]);
        match ring.wait(Some(cursor), 100, 100, Duration::ZERO, &all) {
            TailRead::Batch { spans, .. } => {
                assert_eq!(spans.len(), 1, "the late span must be delivered");
                assert_eq!(spans[0].span_id, "b");
            }
            TailRead::Gap { .. } => panic!("nothing was evicted"),
        }
    }

    #[test]
    fn a_cursor_never_replays_what_it_has_seen() {
        let ring = TailChannel::new(16, usize::MAX);
        ring.publish(&[span("a", 1), span("b", 2)]);
        let mut cursor = match ring.wait(None, 100, 100, Duration::ZERO, &all) {
            TailRead::Batch { spans, cursor } => {
                assert_eq!(spans.len(), 2);
                cursor
            }
            TailRead::Gap { .. } => panic!("no gap"),
        };
        for _ in 0..5 {
            match ring.wait(Some(cursor), 100, 100, Duration::ZERO, &all) {
                TailRead::Batch {
                    spans,
                    cursor: next,
                } => {
                    assert!(spans.is_empty(), "a settled cursor is silent");
                    cursor = next;
                }
                TailRead::Gap { .. } => panic!("no gap"),
            }
        }
    }

    #[test]
    fn equal_timestamps_do_not_stall_the_cursor() {
        // 1,250 spans at ONE timestamp: the burst that no event-time watermark
        // can separate, because every one of them is `>= since`. Sequence
        // numbers are unique per admission, so the burst pages like any other.
        let ring = TailChannel::new(4_096, usize::MAX);
        let burst: Vec<_> = (0..1_250).map(|i| span(&format!("s{i}"), 5_000)).collect();
        ring.publish(&burst);

        let mut cursor = None;
        let mut drained = 0;
        for _ in 0..20 {
            match ring.wait(cursor, 2_000, 200, Duration::ZERO, &all) {
                TailRead::Batch {
                    spans,
                    cursor: next,
                } => {
                    drained += spans.len();
                    cursor = Some(next);
                }
                TailRead::Gap { .. } => panic!("nothing was evicted"),
            }
        }
        assert_eq!(drained, 1_250);
    }

    #[test]
    fn a_zero_backfill_starts_at_the_head() {
        // `backfill` is what a subscriber asks for as opening history, and 0
        // means "only what arrives from now on". The distinction matters
        // because the ring always holds a backlog — without it, every new
        // subscriber would be handed thousands of spans it did not ask for.
        let ring = TailChannel::new(64, usize::MAX);
        ring.publish(&[span("old", 1), span("older", 2)]);
        let cursor = match ring.wait(None, 0, 100, Duration::ZERO, &all) {
            TailRead::Batch { spans, cursor } => {
                assert!(spans.is_empty(), "history is not replayed at backfill 0");
                cursor
            }
            TailRead::Gap { .. } => panic!("no gap"),
        };
        ring.publish(&[span("new", 3)]);
        match ring.wait(Some(cursor), 0, 100, Duration::ZERO, &all) {
            TailRead::Batch { spans, .. } => {
                assert_eq!(spans.len(), 1);
                assert_eq!(spans[0].span_id, "new");
            }
            TailRead::Gap { .. } => panic!("no gap"),
        }
    }

    #[test]
    fn falling_behind_the_ring_reports_how_much_was_lost_and_no_position() {
        let ring = TailChannel::new(4, usize::MAX);
        ring.publish(&[span("a", 1), span("b", 2)]);
        let cursor = match ring.wait(None, 100, 100, Duration::ZERO, &all) {
            TailRead::Batch { cursor, .. } => cursor,
            TailRead::Gap { .. } => panic!("no gap yet"),
        };
        assert_eq!(cursor.seq, 2);
        // Turn the ring over completely.
        for i in 0..8 {
            ring.publish(&[span(&format!("x{i}"), 10 + i)]);
        }
        match ring.wait(Some(cursor), 100, 100, Duration::ZERO, &all) {
            TailRead::Gap { missed } => {
                // Ten admitted, four retained, so the floor is 6 and everything
                // from position 2 to 5 is gone: four admissions.
                assert_eq!(missed, Some(4), "says how much was lost");
            }
            TailRead::Batch { .. } => panic!("evicted entries cannot be delivered"),
        }
    }

    #[test]
    fn a_gap_from_another_process_reports_no_count() {
        // Sequence numbers from a previous process index a different numbering,
        // so subtracting them would produce a confident number that means
        // nothing. `None` is the honest answer.
        let ring = TailChannel::new(8, usize::MAX);
        ring.publish(&[span("a", 1)]);
        match ring.wait(
            Some(TailCursor { epoch: 1, seq: 900 }),
            100,
            100,
            Duration::ZERO,
            &all,
        ) {
            TailRead::Gap { missed } => assert_eq!(missed, None),
            TailRead::Batch { .. } => panic!("a foreign cursor must gap"),
        }
    }

    #[test]
    fn the_byte_budget_evicts_before_the_count_does() {
        // The regression this exists for: a count-only bound let 8,192 spans
        // carrying LLM prompts reach hundreds of megabytes, because the ring is
        // the sole owner of a span once a seal has dropped it from the write
        // buffer. Capacity here is 1,000 and would never bind.
        let ring = TailChannel::new(1_000, 64 * 1024);
        let big = "x".repeat(16 * 1024);
        for i in 0..64 {
            let mut wide = span(&format!("w{i}"), i as u64);
            std::sync::Arc::get_mut(&mut wide)
                .expect("sole handle")
                .attributes
                .insert("prompt".into(), Value::String(big.clone()));
            ring.publish(&[wide]);
        }

        let (spans, bytes, max_spans, max_bytes) = ring.usage().expect("usage");
        assert_eq!(max_spans, 1_000);
        assert_eq!(max_bytes, 64 * 1024);
        assert!(
            spans < 10,
            "the byte budget must bind long before the count: {spans} retained"
        );
        assert!(
            bytes <= 64 * 1024 + 17 * 1024,
            "residency stays at the budget plus at most the newest span: {bytes}"
        );
    }

    #[test]
    fn one_oversized_span_is_still_delivered() {
        // A span larger than the whole budget must not evict itself, or a tail
        // watching a store of large spans would show nothing at all.
        let ring = TailChannel::new(100, 1_024);
        let mut huge = span("huge", 1);
        std::sync::Arc::get_mut(&mut huge)
            .expect("sole handle")
            .attributes
            .insert("prompt".into(), Value::String("x".repeat(64 * 1024)));
        ring.publish(&[huge]);

        match ring.wait(None, 100, 100, Duration::ZERO, &all) {
            TailRead::Batch { spans, .. } => {
                assert_eq!(spans.len(), 1, "the newest entry is never evicted");
                assert_eq!(spans[0].span_id, "huge");
            }
            TailRead::Gap { .. } => panic!("no gap"),
        }
    }

    #[test]
    fn size_tracks_text_rather_than_field_count() {
        let narrow = span("a", 1);
        let mut wide = span("b", 2);
        std::sync::Arc::get_mut(&mut wide)
            .expect("sole handle")
            .attributes
            .insert("prompt".into(), Value::String("x".repeat(32 * 1024)));
        let small = approximate_bytes(&narrow);
        let large = approximate_bytes(&wide);
        assert!(
            large > small + 32_000,
            "the estimate must follow text: {small} vs {large}"
        );
    }

    #[test]
    fn a_cursor_from_another_process_gaps_rather_than_skipping() {
        let ring = TailChannel::new(8, usize::MAX);
        ring.publish(&[span("a", 1)]);
        let stale = TailCursor { epoch: 1, seq: 0 };
        match ring.wait(Some(stale), 100, 100, Duration::ZERO, &all) {
            TailRead::Gap { .. } => {}
            TailRead::Batch { .. } => {
                panic!("a pre-restart cursor must not be read as a live position")
            }
        }
    }

    #[test]
    fn a_filtered_subscriber_still_tracks_the_ring() {
        // Everything is rejected, so the batch is always empty — but the
        // cursor has to keep moving, or the subscriber falls off the back of
        // the ring and gaps on traffic it never asked for.
        let ring = TailChannel::new(4, usize::MAX);
        ring.publish(&[span("a", 1), span("b", 2)]);
        let none = |_: &Span| false;
        let cursor = match ring.wait(None, 100, 100, Duration::ZERO, &none) {
            TailRead::Batch { spans, cursor } => {
                assert!(spans.is_empty());
                assert_eq!(cursor.seq, 2, "consumed both, matched neither");
                cursor
            }
            TailRead::Gap { .. } => panic!("no gap"),
        };
        for i in 0..4 {
            ring.publish(&[span(&format!("x{i}"), 10 + i)]);
        }
        match ring.wait(Some(cursor), 100, 100, Duration::ZERO, &none) {
            TailRead::Batch { cursor, .. } => assert_eq!(cursor.seq, 6),
            TailRead::Gap { .. } => panic!("the cursor kept pace, so it cannot gap"),
        }
    }

    /// A span whose weight is STRUCTURE rather than text: nested objects and
    /// arrays of scalars, no long strings anywhere.
    fn structured_span(id: &str, width: usize, depth: usize) -> Arc<Span> {
        fn nest(depth: usize, width: usize) -> Value {
            if depth == 0 {
                return Value::Array((0..width).map(|n| Value::from(n as u64)).collect());
            }
            let mut object = serde_json::Map::new();
            for n in 0..width {
                object.insert(format!("k{n}"), nest(depth - 1, width));
            }
            Value::Object(object)
        }
        let mut span = span(id, 1);
        let mutable = Arc::get_mut(&mut span).expect("sole owner");
        mutable
            .attributes
            .insert("payload".into(), nest(depth, width));
        span
    }

    #[test]
    fn structured_json_counts_against_the_byte_budget() {
        // The budget has to bound MEMORY, not text. Counting only string bytes
        // — scalars free, containers eight bytes an element — left the ceiling
        // bypassable by shape: deeply structured JSON with no long strings in
        // it measured at a fraction of what it held, so the ring kept
        // accepting spans while the accounting insisted it was nearly empty.
        let heavy = structured_span("s", 8, 3);
        let measured = approximate_bytes(&heavy);

        // Every leaf scalar occupies a whole `Value` slot, and every map entry
        // costs a key plus a slot plus node overhead. 8^3 objects of 8 keys
        // each, bottoming out in 8-element arrays, cannot honestly measure as
        // a few hundred bytes.
        let leaves = 8_usize.pow(4);
        assert!(
            measured >= leaves * std::mem::size_of::<Value>(),
            "structure must be counted: {measured} bytes for {leaves} leaves"
        );

        // And the ring must actually stop on it. A budget of ten spans' worth
        // has to evict at roughly ten spans, whatever the spans are made of.
        let budget = measured * 10;
        let ring = TailChannel::new(100_000, budget);
        for n in 0..200 {
            ring.publish(&[structured_span(&format!("s{n}"), 8, 3)]);
        }
        let (spans, bytes, _, _) = ring.usage().expect("usage");
        assert!(
            bytes <= budget,
            "the ring exceeded its byte budget: {bytes} > {budget}"
        );
        assert!(
            spans <= 12,
            "count should be bounded by BYTES here, not by the 100k cap: {spans}"
        );
    }

    #[test]
    fn spare_collection_capacity_counts_against_the_byte_budget() {
        // A `Vec` grown large and then truncated keeps its whole allocation.
        // Charging `len()` let a caller build a 100,000-element array, cut it
        // to one, and hand the retained allocation to `Store::ingest`: 32 such
        // spans held 100 MB while the ring's accounting reported 13 KB against
        // a 32 MiB budget. The ring keeps the `Vec`, so the ring must be
        // charged for the `Vec`.
        let mut wide = Vec::with_capacity(100_000);
        wide.extend((0..100_000_u64).map(Value::from));
        wide.truncate(1);
        assert!(wide.capacity() >= 100_000, "the allocation is retained");

        let mut span = span("s", 1);
        Arc::get_mut(&mut span)
            .expect("sole owner")
            .attributes
            .insert("wide".into(), Value::Array(wide));

        let measured = approximate_bytes(&span);
        assert!(
            measured >= 100_000 * std::mem::size_of::<Value>(),
            "retained capacity must be counted: {measured} bytes"
        );

        // And the ring evicts on it rather than filling with dead capacity.
        let budget = measured * 4;
        let ring = TailChannel::new(100_000, budget);
        for index in 0..40 {
            let mut wide = Vec::with_capacity(100_000);
            wide.extend((0..100_000_u64).map(Value::from));
            wide.truncate(1);
            let mut one = span_with_id(&format!("s{index}"));
            Arc::get_mut(&mut one)
                .expect("sole owner")
                .attributes
                .insert("wide".into(), Value::Array(wide));
            ring.publish(&[one]);
        }
        let (spans, bytes, _, _) = ring.usage().expect("usage");
        assert!(
            bytes <= budget,
            "ring exceeded its budget: {bytes} > {budget}"
        );
        assert!(
            spans <= 5,
            "bounded by bytes, not by the 100k count cap: {spans}"
        );
    }

    #[test]
    fn a_token_survives_a_round_trip() {
        let cursor = TailCursor {
            epoch: 1_700_000_000_000_000_000,
            seq: 42,
        };
        assert_eq!(TailCursor::parse(&cursor.to_token()), Some(cursor));
        assert_eq!(TailCursor::parse("nonsense"), None);
        assert_eq!(TailCursor::parse("1."), None);
    }

    #[test]
    fn a_waiting_subscriber_wakes_when_a_span_lands() {
        use std::sync::Arc as StdArc;
        let ring = StdArc::new(TailChannel::new(16, usize::MAX));
        let head = ring.head().expect("fresh ring");

        let writer = StdArc::clone(&ring);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            writer.publish(&[span("late", 1)]);
        });

        // A generous timeout that must NOT be reached: the point is that the
        // condvar delivers on the publish, not on the deadline.
        let started = std::time::Instant::now();
        let read = ring.wait(Some(head), 0, 100, Duration::from_secs(5), &all);
        let waited = started.elapsed();
        handle.join().expect("writer thread");

        match read {
            TailRead::Batch { spans, .. } => {
                assert_eq!(spans.len(), 1);
                assert_eq!(spans[0].span_id, "late");
            }
            TailRead::Gap { .. } => panic!("no gap"),
        }
        assert!(
            waited < Duration::from_secs(4),
            "woke on the publish, not the timeout (waited {waited:?})"
        );
    }
}
