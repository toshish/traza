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
//! The ring holds `Arc<Span>` — the same handles [`crate::WriteBuffer`] holds —
//! so a span in both costs one pointer here, not a copy. Entries outlive the
//! buffer's eviction, which is the point: a span sealed into a segment two
//! seconds ago is still what a tail wants to show.

use crate::Span;
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
    /// reconstructed from it.
    ///
    /// Carries the oldest position still retained rather than the newest, so a
    /// client's backfill only has to cover the entries actually dropped.
    Gap {
        /// Where to resume once the gap has been backfilled by another means.
        cursor: TailCursor,
    },
}

/// A bounded ring of recently admitted spans.
pub struct TailRing {
    epoch: u64,
    /// The sequence number of `entries.front()`; when empty, the number the
    /// next push will take.
    ///
    /// Sequence numbers are contiguous, so storing one per entry would be eight
    /// bytes of redundancy that can disagree with the ring's actual contents.
    /// The seq of `entries[i]` is `first_seq + i`, by construction.
    first_seq: u64,
    entries: VecDeque<Arc<Span>>,
    capacity: usize,
}

impl TailRing {
    /// A ring retaining at most `capacity` spans.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            epoch: process_epoch(),
            first_seq: 0,
            entries: VecDeque::with_capacity(capacity.min(4_096)),
            capacity,
        }
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

    /// Appends one admitted span, evicting the oldest if the ring is full.
    fn push(&mut self, span: Arc<Span>) {
        self.entries.push_back(span);
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
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
                        cursor: self.floor(),
                    };
                }
                (cursor.seq - self.first_seq) as usize
            }
        };

        let mut spans = Vec::new();
        let mut consumed = 0_usize;
        for span in self.entries.iter().skip(start) {
            if spans.len() == limit {
                break;
            }
            consumed += 1;
            if keep(span) {
                spans.push(Arc::clone(span));
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
    /// A channel over a ring retaining at most `capacity` spans.
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: Mutex::new(TailRing::new(capacity)),
            signal: Condvar::new(),
        }
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
        let waiting = match ring.read(cursor, backfill, limit, keep) {
            TailRead::Gap { cursor } => return TailRead::Gap { cursor },
            found @ TailRead::Batch { .. } => {
                if advanced(&found, cursor) {
                    return found;
                }
                found
            }
        };
        let resume = match &waiting {
            TailRead::Batch { cursor, .. } => Some(*cursor),
            TailRead::Gap { cursor } => Some(*cursor),
        };
        let (ring, _) = match self.signal.wait_timeout(ring, timeout) {
            Ok(pair) => pair,
            Err(_) => return unchanged(),
        };
        ring.read(resume, backfill, limit, keep)
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

    fn all(_: &Span) -> bool {
        true
    }

    #[test]
    fn delivers_a_span_that_started_before_the_one_already_seen() {
        // The bug this module exists for. Under event-time paging, admitting a
        // span that STARTED earlier than one already delivered leaves it
        // permanently invisible: it sorts below the watermark the client has
        // moved past. Admission order has no such hole.
        let ring = TailChannel::new(16);
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
        let ring = TailChannel::new(16);
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
        let ring = TailChannel::new(4_096);
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
        let ring = TailChannel::new(64);
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
    fn falling_behind_the_ring_reports_a_gap_at_the_floor() {
        let ring = TailChannel::new(4);
        ring.publish(&[span("a", 1), span("b", 2)]);
        let cursor = match ring.wait(None, 100, 100, Duration::ZERO, &all) {
            TailRead::Batch { cursor, .. } => cursor,
            TailRead::Gap { .. } => panic!("no gap yet"),
        };
        // Turn the ring over completely.
        for i in 0..8 {
            ring.publish(&[span(&format!("x{i}"), 10 + i)]);
        }
        match ring.wait(Some(cursor), 100, 100, Duration::ZERO, &all) {
            TailRead::Gap { cursor: floor } => {
                // The floor, not the head: the client backfills only what was
                // actually dropped.
                assert_eq!(floor.seq, 6, "oldest retained of 10 admitted, cap 4");
            }
            TailRead::Batch { .. } => panic!("evicted entries cannot be delivered"),
        }
    }

    #[test]
    fn a_cursor_from_another_process_gaps_rather_than_skipping() {
        let ring = TailChannel::new(8);
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
        let ring = TailChannel::new(4);
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
        let ring = StdArc::new(TailChannel::new(16));
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
