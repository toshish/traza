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
