//! Sealing runs with no engine lock held. These are the tests that can tell.
//!
//! The seal is the largest thing this engine does, and it now happens while
//! other threads ingest and read. Two claims follow from that, and neither is
//! visible from query results on a quiet store:
//!
//! - **Nothing acknowledged ever stops being readable.** A seal that removed
//!   its spans from the write buffer before the segment landed would leave
//!   them in neither place for the whole of the write. Every read would still
//!   return a *valid* answer — just one missing spans the caller was already
//!   promised.
//! - **The work is actually off the lock.** A correctness test cannot see this
//!   at all: the same spans come back whether the seal holds the writer lock
//!   for 30 ms or 0.3 ms. Only instrumentation can, which is what
//!   `traza_segment_seal_locked` exists for.
//!
//! `tests/storage.rs::reads_never_miss_committed_spans` covers neither. Its
//! writer is single-threaded and its `flush` is synchronous end to end, so the
//! window it would need to observe never opens.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use traza::{Config, Durability, Span, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-sealrace-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A span big enough that sealing a buffer full of them is real work — the
/// window under test is the duration of a segment write, and a corpus of empty
/// spans seals too fast to race against.
fn span(trace: &str, id: &str, name: &str) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace,
        "span_id": id,
        "name": name,
        "service": "ingest",
        "start_time_ns": 1_000_000_000u64,
        "end_time_ns": 1_000_001_000u64,
        "attributes": {
            "gen.ai.system": "openai",
            "gen.ai.request.model": "gpt-4o-mini",
            "detail": "x".repeat(256),
        }
    }))
    .expect("span")
}

fn store(dir: &PathBuf, flush_spans: usize, durability: Durability) -> Arc<Store> {
    Arc::new(
        Store::open(
            dir,
            Config {
                flush_spans,
                durability,
                compaction: None,
                ..Config::default()
            },
        )
        .expect("open store"),
    )
}

#[test]
fn reads_see_every_acknowledged_span_while_a_seal_is_in_flight() {
    // THE regression test for an unlocked seal. Ingest and reads run against
    // the store throughout, and the reader checks the exact invariant
    // `get_trace` documents: everything acknowledged before the read started
    // is in the answer.
    //
    // It fails if the seal drops its spans from the write buffer at drain time
    // instead of after the segment is published, because for the duration of
    // the write those spans are in neither the buffer nor a segment.
    const BATCHES: u64 = 40;
    const PER_BATCH: u64 = 250;

    let dir = test_dir("visibility");
    let store = store(&dir, 1_000, Durability::Wal);

    let acknowledged = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let writer = {
        let store = Arc::clone(&store);
        let acknowledged = Arc::clone(&acknowledged);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            for batch in 0..BATCHES {
                let spans: Vec<Span> = (0..PER_BATCH)
                    .map(|item| {
                        let index = batch * PER_BATCH + item;
                        span("race", &format!("span-{index:06}"), "acknowledged")
                    })
                    .collect();
                store.ingest_batch(spans).expect("ingest batch");
                // Published only after the call returned, so a reader that
                // sees this count was genuinely promised those spans.
                acknowledged.store((batch + 1) * PER_BATCH, Ordering::Release);
            }
            done.store(true, Ordering::Release);
        })
    };

    let readers: Vec<_> = (0..3)
        .map(|_| {
            let store = Arc::clone(&store);
            let acknowledged = Arc::clone(&acknowledged);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut reads = 0u64;
                loop {
                    let promised = acknowledged.load(Ordering::Acquire);
                    let spans = store.get_trace("race").expect("get_trace");
                    let seen: std::collections::HashSet<&str> =
                        spans.iter().map(|span| span.span_id.as_str()).collect();
                    for index in 0..promised {
                        assert!(
                            seen.contains(format!("span-{index:06}").as_str()),
                            "span-{index:06} was acknowledged before this read started \
                             and is missing from it: a seal took it out of the write \
                             buffer before its segment was published \
                             ({} of {promised} present)",
                            seen.len()
                        );
                    }
                    reads += 1;
                    if done.load(Ordering::Acquire) && promised == BATCHES * PER_BATCH {
                        return reads;
                    }
                }
            })
        })
        .collect();

    writer.join().expect("writer");
    let reads: u64 = readers.into_iter().map(|r| r.join().expect("reader")).sum();
    // A run in which nothing overlapped would prove nothing, so say so.
    assert!(
        reads >= 3,
        "readers must have actually observed the store during ingest, got {reads}"
    );
    assert!(
        store.metrics().segment_seal.count() >= 5,
        "the run must contain several seals to have raced against, got {}",
        store.metrics().segment_seal.count()
    );

    store.flush().expect("flush");
    assert_eq!(
        store.get_trace("race").expect("get_trace").len() as u64,
        BATCHES * PER_BATCH
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_seal_holds_an_engine_lock_for_a_small_fraction_of_its_work() {
    // The performance claim, asserted through the only thing that can see it.
    // Query results are identical whether or not the write happens under the
    // writer lock, so this reads the instrumentation instead.
    let dir = test_dir("off-lock");
    let store = store(&dir, 2_000, Durability::Wal);

    for batch in 0..20u64 {
        let spans: Vec<Span> = (0..1_000)
            .map(|item| {
                let index = batch * 1_000 + item;
                span("offlock", &format!("span-{index:06}"), "work")
            })
            .collect();
        store.ingest_batch(spans).expect("ingest");
    }
    store.flush().expect("flush");

    let metrics = store.metrics();
    let total = metrics.segment_seal.total_ns();
    let locked = metrics.segment_seal_locked.total_ns();
    let reconcile = metrics.segment_seal_reconcile.total_ns();
    let seals = metrics.segment_seal.count();
    assert!(
        seals >= 5,
        "need several seals to average over, got {seals}"
    );

    // The claim: the segment write happens with no engine lock held. What
    // brackets it is the drain and the publish, and those are the only things
    // `segment_seal_locked` measures.
    //
    // This assertion used to include the post-publish reconcile too, and that
    // made it both wrong and flaky. The reconcile truncates the write-ahead
    // log under the writer lock, so it is an FSYNC — measured here at about
    // 12 ms against a 4 us drain, a factor of three thousand. It therefore set
    // the ratio almost single-handedly, parking it at roughly 23% against a
    // 25% threshold, where it passed or failed on how busy the machine was
    // and would have passed just the same with the segment write back under
    // the lock. Measuring the two separately puts the real figure at ~0.05%,
    // and the 1% threshold below is a twenty-fold margin over that rather
    // than a five-percent one.
    assert!(
        locked * 100 < total,
        "the drain and publish held an engine lock for {locked} ns of {total} ns of seal work — \
         the segment write is supposed to happen with nothing held"
    );

    // And the reconcile stays a bounded, fsync-shaped cost rather than
    // becoming span-proportional work: one log reclamation per seal, not a
    // re-encode of the buffer. Loose on purpose — this one IS an fsync and so
    // it tracks the device, not the code — but it still catches the segment
    // write migrating into the reconcile, which would take it to ~100%.
    assert_eq!(
        metrics.segment_seal_reconcile.count(),
        seals,
        "every seal reconciles exactly once"
    );
    assert!(
        reconcile * 2 < total,
        "the post-publish reconcile held the writer lock for {reconcile} ns of {total} ns — \
         it is supposed to be one log fsync, not the seal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_write_buffer_stays_bounded_when_ingest_outruns_sealing() {
    // Sealing under the writer lock throttled ingest for free — no batch could
    // be admitted while a seal ran. Off the lock that is gone, and gone in the
    // one direction that matters: an ingest that sustainably outruns sealing
    // would grow the buffer until the process died. Past a bounded overshoot
    // an ingesting thread waits for the seal permit instead of skipping.
    const FLUSH_SPANS: usize = 1_000;
    let dir = test_dir("backpressure");
    let store = store(&dir, FLUSH_SPANS, Durability::Wal);

    let workers: Vec<_> = (0..6u64)
        .map(|worker| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let mut high_water = 0usize;
                for batch in 0..40u64 {
                    let spans: Vec<Span> = (0..500)
                        .map(|item| {
                            span(
                                "flood",
                                &format!("w{worker}-b{batch:03}-{item:03}"),
                                "flood",
                            )
                        })
                        .collect();
                    store.ingest_batch(spans).expect("ingest");
                    high_water = high_water.max(store.buffered_span_count());
                }
                high_water
            })
        })
        .collect();
    let high_water = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .max()
        .unwrap_or(0);

    // The bound is 4x the threshold plus whatever the threads holding the
    // writer lock at that moment are admitting — six workers of 500 spans
    // each. Anything near that is the throttle working; unbounded growth
    // reaches the full 120,000-span corpus.
    let ceiling = FLUSH_SPANS * 4 + 6 * 500 + FLUSH_SPANS;
    assert!(
        high_water <= ceiling,
        "the write buffer reached {high_water} spans against a {FLUSH_SPANS}-span \
         seal threshold (bound {ceiling}): ingest outran sealing and nothing \
         pushed back"
    );
    store.flush().expect("flush");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn last_write_wins_never_goes_backwards_across_a_seal() {
    // The other half of the visibility rule. A key re-ingested WHILE its older
    // version is being sealed lives in the buffer while the segment holds the
    // older one; the buffer outranks segments, so the read is correct — but
    // only if the publish evicts by handle identity. Evicting the key instead
    // drops the newer version and lets the segment's older one become the
    // answer, which this sees as a version going backwards.
    let dir = test_dir("monotonic");
    let store = store(&dir, 1_000, Durability::Wal);
    let done = Arc::new(AtomicBool::new(false));
    let version = Arc::new(AtomicU64::new(0));

    // Filler keeps seals running underneath the hot key.
    let filler = {
        let store = Arc::clone(&store);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut index = 0u64;
            while !done.load(Ordering::Acquire) {
                let spans: Vec<Span> = (0..200)
                    .map(|item| {
                        span(
                            "filler",
                            &format!("filler-{:07}", index * 200 + item),
                            "filler",
                        )
                    })
                    .collect();
                store.ingest_batch(spans).expect("ingest filler");
                index += 1;
            }
        })
    };

    let checker = {
        let store = Arc::clone(&store);
        let version = Arc::clone(&version);
        std::thread::spawn(move || {
            for next in 1..=600u64 {
                store
                    .ingest(span("hot", "hot-key", &format!("v{next:05}")))
                    .expect("ingest hot");
                version.store(next, Ordering::Release);

                let promised = version.load(Ordering::Acquire);
                let spans = store.get_trace("hot").expect("get_trace");
                assert_eq!(spans.len(), 1, "one key, one version");
                let observed: u64 = spans[0].name.trim_start_matches('v').parse().expect("v");
                assert!(
                    observed >= promised,
                    "read back v{observed:05} after v{promised:05} was acknowledged: \
                     a seal evicted the newer buffered version and left the segment's \
                     older one live"
                );
            }
        })
    };

    checker.join().expect("checker");
    done.store(true, Ordering::Release);
    filler.join().expect("filler");

    store.flush().expect("flush");
    let spans = store.get_trace("hot").expect("get_trace");
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].name, "v00600",
        "the last write is the one that won"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flushed_durability_stays_synchronous_under_concurrency() {
    // `flushed` promises a SEALED segment, not an fsync, so its seal may never
    // be skipped because another one happens to be running. Every thread's
    // spans must be on disk by the time its ingest returns.
    let dir = test_dir("flushed-sync");
    let store = store(&dir, 10_000, Durability::Flushed);

    let workers: Vec<_> = (0..4u64)
        .map(|worker| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for round in 0..25u64 {
                    store
                        .ingest(span(
                            "flushed",
                            &format!("w{worker}-r{round:03}"),
                            "acknowledged",
                        ))
                        .expect("ingest");
                    // Buffered spans may exist (other threads are ingesting),
                    // but OURS is acknowledged and must already be sealed.
                    let persisted: usize = store
                        .persisted_segment_spans()
                        .expect("segments")
                        .iter()
                        .flatten()
                        .filter(|span| span.span_id == format!("w{worker}-r{round:03}"))
                        .count();
                    assert_eq!(
                        persisted, 1,
                        "flushed mode acknowledged w{worker}-r{round:03} without sealing it"
                    );
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A span that outlives the cutoff the expiry test uses.
fn fresh_span(trace: &str, id: &str) -> Span {
    let mut span = span(trace, id, "fresh");
    span.start_time_ns = 9_000_000_000;
    span.end_time_ns = 9_000_001_000;
    span
}

#[test]
fn expiry_cannot_be_undone_by_a_seal_that_was_already_running() {
    // The deletion race an unlocked seal creates. A seal drains a snapshot
    // holding expired spans; expiry then removes them from the write buffer,
    // the log and every segment it can see — and the seal publishes its
    // segment afterwards, putting them back where expiry has already been.
    //
    // Expiry takes the seal permit to close it. Without that permit, deleted
    // spans reappear in a segment and nothing removes them again.
    //
    // Each round stages the window deliberately: the buffer is filled to just
    // under the seal threshold with expiring spans, so the very next batch
    // starts a seal carrying all of them, and the deletion is issued while
    // that seal is writing.
    // The threshold is large so the seal it triggers takes long enough that
    // the deletion below reliably lands inside the write rather than before or
    // after it.
    const ROUNDS: u64 = 4;
    const FLUSH_SPANS: usize = 20_000;

    let dir = test_dir("expiry-race");
    let store = store(&dir, FLUSH_SPANS, Durability::Wal);

    for round in 0..ROUNDS {
        // Fill to just under the threshold with spans the cutoff will delete.
        let doomed: Vec<Span> = (0..(FLUSH_SPANS as u64 - 200))
            .map(|item| span("expiring", &format!("doomed-r{round}-{item:06}"), "old"))
            .collect();
        store.ingest_batch(doomed).expect("ingest doomed");

        // This batch crosses the threshold, so it starts a seal whose snapshot
        // holds every doomed span above. The seal runs on a worker thread so
        // the deletion below overlaps its write rather than following it.
        let sealer = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let spans: Vec<Span> = (0..400)
                    .map(|item| fresh_span("surviving", &format!("keep-r{round}-{item:06}")))
                    .collect();
                store.ingest_batch(spans).expect("ingest trigger");
            })
        };

        // Let the seal get past its drain and into the segment write. Without
        // this the deletion usually wins the start, the seal never carries the
        // doomed spans, and the round proves nothing. The assertion below only
        // fires on an actual resurrection, so a round that still loses the
        // race is a missed opportunity rather than a false failure.
        std::thread::sleep(std::time::Duration::from_millis(60));

        // Every doomed span ends at 1_000_001_000 ns; every surviving one at
        // 9_000_001_000. This deletes exactly the first group.
        store.expire_before(2_000_000_000).expect("expire");
        sealer.join().expect("sealer");

        // No second sweep, and no flush: the question is whether the deletion
        // that already ran is still true once the seal it raced has landed.
        let resurrected = store.get_trace("expiring").expect("get_trace");
        assert!(
            resurrected.is_empty(),
            "round {round}: {} expired spans came back after a seal that had \
             already drained them published its segment",
            resurrected.len()
        );
    }

    // The spans that were never supposed to go must all still be here.
    assert_eq!(
        store.get_trace("surviving").expect("get_trace").len() as u64,
        ROUNDS * 400,
        "expiry took spans the cutoff does not cover"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
