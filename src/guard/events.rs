//! The fuse event feed: a bounded in-memory ring of verdict transitions, read
//! by cursor and long-poll.
//!
//! An alert dispatcher wants "every warn, stop and reset since I last asked",
//! not a scan of every live run. Transitions are therefore recorded as they
//! happen — a run entering `warn`, entering `stop`, being manually stopped, or
//! being reset — numbered in the order they happened, and held in a ring of
//! [`RING_CAPACITY`] events. A reader passes back the cursor of the last event
//! it saw and gets what came after, waiting up to a bound for something to
//! arrive when nothing has.
//!
//! Positions are `"<epoch>.<seq>"` exactly like the live tail's: the epoch
//! names this process, because sequence numbers live in memory and restart at
//! zero. Unlike the tail, a position that is no longer usable — another
//! process issued it, or the ring turned over past it — is answered with
//! everything still retained and `gap: true`, rather than with nothing: an
//! event is small, the reader is a dispatcher deciding what to alert on, and
//! the honest recovery for it is "here is what survives, and you missed some".

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Events retained.
pub(crate) const RING_CAPACITY: usize = 4096;

/// Most events one read returns; the cursor it hands back resumes the rest
/// without waiting. Bounds a response, not the ring.
pub(crate) const MAX_EVENTS_PER_READ: usize = 1000;

/// Longest a read may wait for an event, in milliseconds.
pub const MAX_WAIT_MS: u64 = 30_000;

/// What happened to a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The run's verdict became `warn`.
    Warn,
    /// A policy limit stopped the run.
    Stop,
    /// A stop was cleared (and limits possibly raised).
    Reset,
    /// The kill switch: an operator stopped the run.
    ManualStop,
}

/// One verdict transition.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FuseEvent {
    /// This event's position; pass it as `after` to read what follows it.
    pub cursor: String,
    /// When it happened, Unix nanoseconds (server clock).
    pub at_ns: u64,
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The run id.
    pub run: String,
    /// What happened.
    pub kind: EventKind,
    /// Why, as the verdict states it.
    pub reasons: Vec<String>,
    /// The policy the run was held to at the time.
    pub policy: String,
}

/// One read of the feed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EventPage {
    /// Matching events, oldest first.
    pub events: Vec<FuseEvent>,
    /// Where to resume: the position after the last event this read
    /// CONSUMED, matching or not, so a reader filtering to one tenant keeps
    /// pace with the ring instead of falling off its back.
    pub next: String,
    /// Present and `true` when the requested position was unusable and
    /// events may have been missed between it and the first one returned.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gap: bool,
}

/// A position: `epoch.seq`, where `seq` is the LAST event seen (0: none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    epoch: u64,
    seq: u64,
}

impl Cursor {
    fn parse(token: &str) -> Option<Self> {
        let (epoch, seq) = token.split_once('.')?;
        Some(Self {
            epoch: epoch.parse().ok()?,
            seq: seq.parse().ok()?,
        })
    }

    fn token(self) -> String {
        format!("{}.{}", self.epoch, self.seq)
    }
}

struct Entry {
    seq: u64,
    event: FuseEvent,
}

struct Ring {
    epoch: u64,
    /// The sequence number the next event gets; the first is 1, so `epoch.0`
    /// reads from the beginning.
    next_seq: u64,
    /// The last sequence number capacity EVICTED, so the oldest position a
    /// reader can resume from without having missed anything. Events an
    /// erasure removed do not move it: those were withheld, not missed.
    evicted_through: u64,
    entries: VecDeque<Entry>,
    capacity: usize,
}

impl Ring {
    fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// The oldest position still readable without a gap.
    fn floor(&self) -> u64 {
        self.evicted_through
    }

    /// Where a read starting from `after` begins, and whether that is a gap.
    fn start(&self, after: Option<Cursor>) -> (u64, bool) {
        match after {
            None => (self.floor(), false),
            Some(cursor)
                if cursor.epoch == self.epoch
                    && cursor.seq >= self.floor()
                    && cursor.seq <= self.last_seq() =>
            {
                (cursor.seq, false)
            }
            // Another process's numbering, evicted past, or claiming more
            // than was ever emitted (corrupt): resynchronize from what is
            // retained and say so.
            Some(_) => (self.floor(), true),
        }
    }

    /// Events after `from`, up to the read bound; returns them with the seq
    /// of the last one consumed.
    fn read(&self, from: u64, keep: &dyn Fn(&FuseEvent) -> bool) -> (Vec<FuseEvent>, u64) {
        let mut events = Vec::new();
        let mut consumed = from;
        for entry in self.entries.iter().filter(|entry| entry.seq > from) {
            if events.len() == MAX_EVENTS_PER_READ {
                break;
            }
            consumed = entry.seq;
            if keep(&entry.event) {
                events.push(entry.event.clone());
            }
        }
        (events, consumed)
    }
}

/// The ring plus the signal that turns a read into a long-poll.
pub(crate) struct EventFeed {
    ring: Mutex<Ring>,
    signal: Condvar,
}

impl EventFeed {
    /// An empty feed for this process.
    pub(crate) fn new() -> Self {
        Self::with_capacity(RING_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |since| since.as_nanos() as u64);
        Self {
            ring: Mutex::new(Ring {
                epoch,
                next_seq: 1,
                evicted_through: 0,
                entries: VecDeque::new(),
                capacity: capacity.max(1),
            }),
            signal: Condvar::new(),
        }
    }

    /// Records one event, assigning its cursor, and wakes every waiting
    /// reader. A poisoned ring drops the event: losing an alert is bad, but
    /// failing the ingest or the reset that produced it — both already
    /// durable — would be worse.
    pub(crate) fn push(&self, mut event: FuseEvent) {
        if let Ok(mut ring) = self.ring.lock() {
            let seq = ring.next_seq;
            ring.next_seq += 1;
            event.cursor = Cursor {
                epoch: ring.epoch,
                seq,
            }
            .token();
            ring.entries.push_back(Entry { seq, event });
            while ring.entries.len() > ring.capacity {
                if let Some(evicted) = ring.entries.pop_front() {
                    ring.evicted_through = evicted.seq;
                }
            }
        }
        self.signal.notify_all();
    }

    /// Drops every retained event `keep` rejects — what an erasure does to
    /// the feed. Positions stay valid: sequence numbers are not reused, and
    /// a cursor that pointed into a dropped run of events simply resumes
    /// after it.
    pub(crate) fn retain(&self, keep: &dyn Fn(&FuseEvent) -> bool) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.entries.retain(|entry| keep(&entry.event));
        }
    }

    /// Reads the events after `after` that `keep` accepts, waiting up to
    /// `wait` for one to arrive when none is there yet.
    ///
    /// Returns as soon as at least one matching event is available. A read
    /// that consumes only events `keep` rejects keeps waiting — out to the
    /// same deadline — but its cursor still advances past them. A malformed
    /// `after` is the caller's error; an unusable well-formed one is a gap.
    pub(crate) fn read(
        &self,
        after: Option<&str>,
        wait: Duration,
        keep: &dyn Fn(&FuseEvent) -> bool,
    ) -> Result<EventPage, String> {
        let after = match after {
            None => None,
            Some(token) => Some(Cursor::parse(token).ok_or_else(|| {
                format!("after must be a cursor of the form <epoch>.<seq> (got {token:?})")
            })?),
        };
        let deadline = Instant::now() + wait;
        let mut ring = self
            .ring
            .lock()
            .map_err(|_| "the fuse event ring is poisoned".to_owned())?;
        let (mut from, gap) = ring.start(after);
        loop {
            let (events, consumed) = ring.read(from, keep);
            from = consumed;
            let now = Instant::now();
            if !events.is_empty() || gap || now >= deadline {
                return Ok(EventPage {
                    events,
                    next: Cursor {
                        epoch: ring.epoch,
                        seq: from,
                    }
                    .token(),
                    gap,
                });
            }
            ring = match self.signal.wait_timeout(ring, deadline - now) {
                Ok((ring, _)) => ring,
                Err(_) => return Err("the fuse event ring is poisoned".to_owned()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn event(tenant: &str, run: &str, kind: EventKind) -> FuseEvent {
        FuseEvent {
            cursor: String::new(),
            at_ns: 1,
            tenant: tenant.into(),
            run: run.into(),
            kind,
            reasons: vec!["why".into()],
            policy: "global".into(),
        }
    }

    fn all(_: &FuseEvent) -> bool {
        true
    }

    #[test]
    fn a_reader_resumes_after_the_last_event_it_consumed() {
        let feed = EventFeed::new();
        let empty = feed.read(None, Duration::ZERO, &all).expect("reads");
        assert!(empty.events.is_empty() && !empty.gap);
        assert!(empty.next.ends_with(".0"), "{}", empty.next);

        feed.push(event("", "a", EventKind::Warn));
        feed.push(event("", "a", EventKind::Stop));
        let page = feed
            .read(Some(&empty.next), Duration::ZERO, &all)
            .expect("reads");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[1].kind, EventKind::Stop);
        assert_eq!(page.next, page.events[1].cursor);

        feed.push(event("", "a", EventKind::Reset));
        let rest = feed
            .read(Some(&page.next), Duration::ZERO, &all)
            .expect("reads");
        assert_eq!(rest.events.len(), 1);
        assert_eq!(rest.events[0].kind, EventKind::Reset);
        let none = feed
            .read(Some(&rest.next), Duration::ZERO, &all)
            .expect("reads");
        assert!(none.events.is_empty());
        assert_eq!(none.next, rest.next, "an empty read holds its position");
    }

    #[test]
    fn a_filtered_reader_keeps_pace_with_events_it_rejects() {
        let feed = EventFeed::new();
        feed.push(event("other", "x", EventKind::Stop));
        feed.push(event("other", "y", EventKind::Stop));
        let acme = |event: &FuseEvent| event.tenant == "acme";
        let page = feed.read(None, Duration::ZERO, &acme).expect("reads");
        assert!(page.events.is_empty());
        assert!(page.next.ends_with(".2"), "consumed both: {}", page.next);
    }

    #[test]
    fn an_evicted_or_foreign_cursor_is_a_gap_with_what_survives() {
        let feed = EventFeed::with_capacity(3);
        for run in ["a", "b", "c", "d", "e"] {
            feed.push(event("", run, EventKind::Stop));
        }
        let epoch = feed.read(None, Duration::ZERO, &all).expect("reads").next;
        let epoch = epoch.split_once('.').expect("cursor").0.to_owned();

        let evicted = feed
            .read(Some(&format!("{epoch}.1")), Duration::ZERO, &all)
            .expect("reads");
        assert!(evicted.gap);
        let runs: Vec<_> = evicted.events.iter().map(|e| e.run.as_str()).collect();
        assert_eq!(runs, ["c", "d", "e"], "the oldest retained onwards");

        // The floor itself is still continuous: seq 2 was the last evicted,
        // so reading after it misses nothing.
        let edge = feed
            .read(Some(&format!("{epoch}.2")), Duration::ZERO, &all)
            .expect("reads");
        assert!(!edge.gap && edge.events.len() == 3);

        for foreign in ["1.3".to_owned(), format!("{epoch}.99")] {
            let page = feed
                .read(Some(&foreign), Duration::ZERO, &all)
                .expect("reads");
            assert!(page.gap, "{foreign}");
            assert_eq!(page.events.len(), 3, "{foreign}");
        }
        assert!(feed.read(Some("garbage"), Duration::ZERO, &all).is_err());
    }

    #[test]
    fn an_erasure_withholds_events_without_inventing_a_gap() {
        let feed = EventFeed::new();
        for run in ["gone", "kept", "gone"] {
            feed.push(event("", run, EventKind::Stop));
        }
        let start = feed.read(None, Duration::ZERO, &all).expect("reads");
        let epoch = start.next.split_once('.').expect("cursor").0.to_owned();
        feed.retain(&|event: &FuseEvent| event.run != "gone");
        let page = feed
            .read(Some(&format!("{epoch}.0")), Duration::ZERO, &all)
            .expect("reads");
        assert!(!page.gap, "nothing was missed; it was withheld");
        let runs: Vec<_> = page.events.iter().map(|e| e.run.as_str()).collect();
        assert_eq!(runs, ["kept"]);
        // A cursor past the withheld tail is still a valid position.
        let tail = feed
            .read(Some(&format!("{epoch}.3")), Duration::ZERO, &all)
            .expect("reads");
        assert!(!tail.gap && tail.events.is_empty());
    }

    #[test]
    fn a_long_poll_wakes_on_the_next_event() {
        let feed = Arc::new(EventFeed::new());
        let head = feed.read(None, Duration::ZERO, &all).expect("reads").next;
        let pusher = Arc::clone(&feed);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            pusher.push(event("", "late", EventKind::ManualStop));
        });
        // Generous: the wait ends when the event lands, long before this.
        let page = feed
            .read(Some(&head), Duration::from_secs(20), &all)
            .expect("reads");
        handle.join().expect("joins");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].run, "late");
    }
}
