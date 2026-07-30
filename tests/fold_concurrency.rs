//! Aggregations must not stop ingest.
//!
//! `fold_spans` reads the whole corpus. It originally did so while holding the
//! writer and segment locks, which meant every series, duration, failure or
//! slowest query blocked ingestion for the length of a full scan — and the
//! Overview screen starts four of them at once. Nothing caught it, because
//! every existing test either ingests or reads, never both at the same
//! instant.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use traza::{Config, Span, SpanFilter, Store};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("traza-fold-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).expect("dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config() -> Config {
    Config {
        flush_spans: 1_000_000, // keep everything in the buffer; the scan is the point
        ttl_seconds: None,
        payload_threshold: None,
        durability: traza::Durability::Buffered,
        compaction: None,
        wal_commit_window: None,
        content_index: true,
        tail_ring_spans: traza::DEFAULT_TAIL_RING_SPANS,
        tail_ring_bytes: traza::DEFAULT_TAIL_RING_BYTES,
        flush_wal_bytes: None,
    }
}

fn span(index: u64) -> Span {
    let mut attributes = Map::new();
    attributes.insert("index".to_owned(), Value::from(index));
    Span {
        trace_id: format!("t-{index:07}"),
        span_id: "s".to_owned(),
        parent_span_id: None,
        name: "op".to_owned(),
        service: "svc".to_owned(),
        status: "ok".to_owned(),
        start_time_ns: 1_700_000_000_000_000_000 + index,
        end_time_ns: 1_700_000_000_000_000_000 + index + 1_000_000,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

#[test]
fn a_fold_does_not_hold_the_writer_lock() {
    let dir = TestDir::new("no-lock");
    let store = Arc::new(Store::open(dir.0.clone(), config()).expect("open"));

    // Enough spans that a fold is not instantaneous.
    let seed: Vec<Span> = (0..40_000).map(span).collect();
    store.ingest_batch(seed).expect("seed");

    let (started_tx, started_rx) = mpsc::channel();
    let (ingested_tx, ingested_rx) = mpsc::channel();
    let folding = Arc::new(AtomicBool::new(true));

    // A writer that ingests one batch as soon as the fold is under way.
    let writer_store = Arc::clone(&store);
    let writer_flag = Arc::clone(&folding);
    let writer = std::thread::spawn(move || {
        started_rx.recv().expect("fold started");
        // The fold is mid-scan right now. If it holds the writer lock, this
        // blocks until the whole scan finishes.
        let began = Instant::now();
        writer_store
            .ingest_batch(vec![span(9_000_000)])
            .expect("ingest during fold");
        let waited = began.elapsed();
        let still_folding = writer_flag.load(Ordering::SeqCst);
        ingested_tx.send((waited, still_folding)).expect("report");
    });

    // Fold the corpus, signalling once from inside the visitor so the writer
    // starts while the scan is genuinely in flight.
    let mut seen = 0u64;
    let mut signalled = false;
    store
        .fold_spans(&SpanFilter::default(), |_span| {
            seen += 1;
            if !signalled {
                signalled = true;
                let _ = started_tx.send(());
            }
            // Give the writer a real chance to run inside the scan.
            if seen == 1 {
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .expect("fold");
    folding.store(false, Ordering::SeqCst);

    let (waited, during) = ingested_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the writer must not be blocked for the length of the scan");
    writer.join().expect("writer thread");

    assert_eq!(seen, 40_000, "the fold must still see the whole corpus");
    assert!(
        during,
        "the ingest completed only after the fold finished, so the scan was \
         holding the writer lock (waited {waited:?})"
    );

    // And the write landed.
    let found = store
        .query(&SpanFilter {
            attributes: vec![("index".to_owned(), Value::from(9_000_000u64))],
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(found.len(), 1, "the concurrent write must be readable");
}

#[test]
fn a_fold_reads_one_coherent_instant() {
    // A snapshot is not just about locks: it is what makes an aggregate a
    // reading of one state rather than of a moving one. A fold that saw spans
    // ingested while it ran would report totals that matched no instant the
    // store was ever in.
    let dir = TestDir::new("coherent");
    let store = Arc::new(Store::open(dir.0.clone(), config()).expect("open"));
    store
        .ingest_batch((0..5_000).map(span).collect())
        .expect("seed");

    let writer_store = Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        for batch in 0..20u64 {
            let spans: Vec<Span> = (0..100)
                .map(|i| span(1_000_000 + batch * 100 + i))
                .collect();
            writer_store.ingest_batch(spans).expect("ingest");
        }
    });

    let mut counted = 0u64;
    store
        .fold_spans(&SpanFilter::default(), |_| {
            counted += 1;
            if counted % 500 == 0 {
                std::thread::yield_now();
            }
        })
        .expect("fold");
    writer.join().expect("writer");

    // The snapshot was taken before or during the writes, so the count is the
    // seed, or the seed plus whole batches that had already landed — never a
    // torn number, and never fewer than the seed.
    assert!(
        (5_000..=7_000).contains(&counted),
        "fold saw {counted}, which is not a state the store passed through"
    );
}

/// An LLM aggregation must not hold the SEGMENTS lock for its duration.
///
/// This is the sibling of `a_fold_does_not_hold_the_writer_lock`, one lock
/// down. `fold_analytics` walks every segment, decoding the ones a time window
/// only partly covers and reading a sidecar file for the rest — and it did all
/// of that with the segments mutex held. A seal takes the writer lock and then
/// the segments lock, so an aggregation stalled ingest for as long as it ran,
/// and the Overview screen starts several at once.
///
/// Nothing about the aggregate's VALUE can show this: the same rows come back
/// whether the lock was held for 300 ms or 300 ns. So the test races a real
/// seal against a real fold and times the seal.
#[test]
fn an_analytics_fold_does_not_hold_the_segments_lock() {
    use traza::analytics::LlmGroupBy;

    let dir = TestDir::new("segments-lock");
    let store = Arc::new(
        Store::open(
            dir.0.clone(),
            Config {
                flush_spans: 1_000,
                ..config()
            },
        )
        .expect("open"),
    );

    // Every segment must cover the WHOLE time range, so that a partial window
    // straddles all of them and none can be answered from a cached rollup.
    // That is also what real concurrent ingest produces, and it is the case
    // the lock hold actually hurt.
    const SEGMENTS: u64 = 30;
    const PER_SEGMENT: u64 = 1_000;
    let base = 1_700_000_000_000_000_000u64;
    for segment in 0..SEGMENTS {
        let batch: Vec<Span> = (0..PER_SEGMENT)
            .map(|slot| {
                let mut s = span(segment * PER_SEGMENT + slot);
                s.start_time_ns = base + slot * 1_000_000;
                s.end_time_ns = s.start_time_ns + 1_000;
                s
            })
            .collect();
        store.ingest_batch(batch).expect("seed");
        store.flush().expect("seal");
    }

    // A window that covers the middle of every segment, so every segment is
    // decoded rather than rolled up.
    let since = base + (PER_SEGMENT / 4) * 1_000_000;
    let until = base + (PER_SEGMENT * 3 / 4) * 1_000_000;
    let fold_once = |store: &Store| {
        store
            .llm_aggregate(LlmGroupBy::Service, Some(since), Some(until))
            .expect("aggregate")
    };

    // Calibrate: how long does one fold take with nothing competing?
    let solo = Instant::now();
    let rows = fold_once(&store);
    let fold_ns = solo.elapsed();
    assert_eq!(rows.len(), 1, "the corpus has one service");
    assert!(
        fold_ns >= Duration::from_millis(20),
        "the fold must be slow enough to race against, took {fold_ns:?}"
    );

    // Fold continuously in the background.
    let folding = Arc::new(AtomicBool::new(true));
    let folder_store = Arc::clone(&store);
    let folder_flag = Arc::clone(&folding);
    let folder = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut folds = 0u32;
        while Instant::now() < deadline {
            let _ = folder_store
                .llm_aggregate(LlmGroupBy::Service, Some(since), Some(until))
                .expect("aggregate");
            folds += 1;
        }
        folder_flag.store(false, Ordering::SeqCst);
        folds
    });

    // Seal repeatedly while the folds run. A seal takes the segments lock at
    // its drain and again at its publish, so if a fold held that lock these
    // would each wait a whole fold.
    let mut seals = 0u32;
    while folding.load(Ordering::SeqCst) && seals < 40 {
        store
            .ingest_batch(vec![span(9_000_000 + u64::from(seals))])
            .expect("ingest");
        store.flush().expect("flush during fold");
        seals += 1;
    }
    let folds = folder.join().expect("folder thread");

    assert!(seals >= 5, "need several seals to have raced, got {seals}");
    assert!(folds >= 2, "need several folds to have raced, got {folds}");

    // Assert on the WAIT, not on how long a seal took.
    //
    // The obvious test — time `flush()` and compare it against a fold — does
    // not work, and failed intermittently when it was written that way. A
    // seal's wall clock is dominated by its segment write and its
    // write-ahead-log fsync, neither of which has anything to do with this
    // lock; on a busy machine an fsync alone can exceed a fold, so the
    // comparison measured the storage device and blamed the fold.
    // `segments_lock_wait` times exactly the thing under test — every
    // acquisition of that one mutex, by any thread — so a fold holding it
    // across its scan shows up here and nothing else does.
    let metrics = store.metrics();
    let waited = Duration::from_nanos(metrics.segments_lock_wait.percentile_ns(99.0));
    assert!(
        metrics.segments_lock_wait.count() >= 10,
        "need several acquisitions to have been timed, got {}",
        metrics.segments_lock_wait.count()
    );
    // Generous on purpose: the point is the order of magnitude. Holding the
    // lock across the fold puts this at or above one fold; releasing it puts
    // it three orders of magnitude below.
    assert!(
        waited * 4 < fold_ns,
        "the segments lock was waited on for {waited:?} at p99 while one fold \
         takes {fold_ns:?} — the fold is holding it across its scan"
    );
}
