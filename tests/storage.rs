//! End-to-end storage tests covering persistence, recovery, filtering, and expiry.

use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use traza::{Config, Error, Span, SpanCursor, SpanFilter, Store};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let serial = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "traza-storage-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn span(trace_id: &str, span_id: String, start_time_ns: u64, duration_ns: u64) -> Span {
    Span {
        trace_id: trace_id.to_owned(),
        span_id,
        parent_span_id: None,
        name: "operation".to_owned(),
        start_time_ns,
        end_time_ns: start_time_ns + duration_ns,
        status: "ok".to_owned(),
        service: "test-service".to_owned(),
        attributes: Map::new(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

fn queried_ids(store: &Store, filter: &SpanFilter) -> Vec<String> {
    let mut ids: Vec<_> = store
        .query(filter)
        .expect("query store")
        .into_iter()
        .map(|span| span.span_id)
        .collect();
    ids.sort();
    ids
}

#[test]
fn buffer_flush_persists_sorted_batches() {
    let dir = TestDir::new("buffer-flush");
    let store = Store::open(
        dir.path(),
        Config {
            flush_spans: 8,
            ttl_seconds: None,
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            flush_wal_bytes: None,
        },
    )
    .expect("open store");

    let shuffled_starts = [
        29, 3, 17, 8, 25, 1, 13, 21, 6, 19, 11, 27, 0, 15, 23, 9, 28, 4, 18, 7, 24, 2, 14, 20, 5,
        16, 10, 26, 12, 22,
    ];
    let mut expected_ids = Vec::new();
    for (index, start) in shuffled_starts.into_iter().enumerate() {
        let span_id = format!("batch-{index:03}");
        expected_ids.push(span_id.clone());
        store
            .ingest(span("batch-trace", span_id, start * 1_000, 100))
            .expect("ingest span");
    }

    store.flush().expect("flush store");
    assert_eq!(store.buffered_span_count(), 0);
    assert!(store.stats().expect("read stats").segment_count > 0);

    expected_ids.sort();
    assert_eq!(queried_ids(&store, &SpanFilter::default()), expected_ids);

    // Inspect segment boundaries directly: a globally sorted query can hide an
    // incorrectly ordered on-disk batch.
    let persisted_segments = store
        .persisted_segment_spans()
        .expect("inspect persisted segments");
    for segment in persisted_segments {
        assert!(segment
            .windows(2)
            .all(|pair| pair[0].start_time_ns <= pair[1].start_time_ns));
    }
}

#[test]
fn crash_recovery_preserves_flushed_spans() {
    let dir = TestDir::new("recovery");
    let flushed_ids: Vec<_> = (0..12).map(|i| format!("flushed-{i:03}")).collect();
    let unflushed_ids: Vec<_> = (0..3).map(|i| format!("unflushed-{i:03}")).collect();

    {
        let store = Store::open(
            dir.path(),
            Config {
                flush_spans: 32,
                ttl_seconds: None,
                payload_threshold: None,
                durability: traza::Durability::Buffered,
                compaction: None,
                wal_commit_window: None,
                flush_wal_bytes: None,
            },
        )
        .expect("open initial store");

        for (index, span_id) in flushed_ids.iter().enumerate() {
            store
                .ingest(span(
                    "recovery-trace",
                    span_id.clone(),
                    1_000 + index as u64,
                    10,
                ))
                .expect("ingest flushed span");
        }
        store.flush().expect("persist known batch");

        for (index, span_id) in unflushed_ids.iter().enumerate() {
            store
                .ingest(span(
                    "unflushed-trace",
                    span_id.clone(),
                    10_000 + index as u64,
                    10,
                ))
                .expect("ingest unflushed span");
        }
        assert_eq!(store.buffered_span_count(), unflushed_ids.len());

        // TODO: add a child-process kill test once cross-process crash orchestration is available.
    }

    let reopened = Store::open(
        dir.path(),
        Config {
            flush_spans: 32,
            ttl_seconds: None,
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            flush_wal_bytes: None,
        },
    )
    .expect("reopen store after dropping unflushed data");

    let recovered: HashSet<_> = reopened
        .query(&SpanFilter::default())
        .expect("query recovered store")
        .into_iter()
        .map(|span| span.span_id)
        .collect();
    for span_id in &flushed_ids {
        assert!(
            recovered.contains(span_id),
            "missing flushed span {span_id}"
        );
    }
    for span_id in &unflushed_ids {
        assert!(
            !recovered.contains(span_id),
            "unexpected recovery of unflushed span {span_id}"
        );
    }
    assert_eq!(recovered.len(), flushed_ids.len());
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, upper: u64) -> u64 {
        self.next() % upper
    }
}

fn naive_matches(span: &Span, filter: &SpanFilter) -> bool {
    if let Some(service) = &filter.service {
        if &span.service != service {
            return false;
        }
    }
    if let Some(name) = &filter.name {
        if &span.name != name {
            return false;
        }
    }
    for (key, expected) in &filter.attributes {
        if span.attributes.get(key) != Some(expected) {
            return false;
        }
    }
    if let Some(min_duration_ns) = filter.min_duration_ns {
        if span.end_time_ns.saturating_sub(span.start_time_ns) < min_duration_ns {
            return false;
        }
    }
    if let Some(since_ns) = filter.since_ns {
        if span.start_time_ns < since_ns {
            return false;
        }
    }
    if let Some(until_ns) = filter.until_ns {
        if span.start_time_ns > until_ns {
            return false;
        }
    }
    true
}

fn naive_query_ids(spans: &[Span], filter: &SpanFilter) -> Vec<String> {
    let mut matching: Vec<_> = spans
        .iter()
        .filter(|span| naive_matches(span, filter))
        .collect();
    matching.sort_by_key(|span| span.start_time_ns);
    if let Some(limit) = filter.limit {
        matching.truncate(limit);
    }
    let mut ids: Vec<_> = matching
        .into_iter()
        .map(|span| span.span_id.clone())
        .collect();
    ids.sort();
    ids
}

#[test]
fn randomized_filters_match_naive_reference() {
    const SERVICES: [&str; 5] = ["api", "billing", "catalog", "search", "worker"];
    const NAMES: [&str; 8] = [
        "accept",
        "authorize",
        "decode",
        "fetch",
        "index",
        "publish",
        "render",
        "write",
    ];
    const REGIONS: [&str; 4] = ["east", "north", "south", "west"];

    let dir = TestDir::new("randomized-filters");
    let store = Store::open(
        dir.path(),
        Config {
            flush_spans: 4_096,
            ttl_seconds: None,
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            flush_wal_bytes: None,
        },
    )
    .expect("open store");
    let mut rng = Lcg(0x5eed_cafe_f00d_1234);
    let mut spans = Vec::with_capacity(2_000);

    for index in 0..2_000_u64 {
        let service = SERVICES[rng.below(SERVICES.len() as u64) as usize].to_owned();
        let name = NAMES[rng.below(NAMES.len() as u64) as usize].to_owned();
        let start_time_ns = 1_000_000 + index * 1_000 + rng.below(900);
        let duration_ns = 1 + rng.below(20_000);
        let mut attributes = Map::new();
        attributes.insert(
            "region".to_owned(),
            Value::String(REGIONS[rng.below(REGIONS.len() as u64) as usize].to_owned()),
        );
        attributes.insert("bucket".to_owned(), Value::from(rng.below(6)));
        attributes.insert("sampled".to_owned(), Value::Bool(rng.below(2) == 0));
        spans.push(Span {
            trace_id: format!("trace-{:04}", index / 4),
            span_id: format!("random-{index:04}"),
            parent_span_id: None,
            name,
            start_time_ns,
            end_time_ns: start_time_ns + duration_ns,
            status: if rng.below(5) == 0 { "error" } else { "ok" }.to_owned(),
            service,
            attributes,
            events: Vec::new(),
            links: Vec::new(),
            extra: serde_json::Map::new(),
        });
    }

    store
        .ingest_batch(spans.clone())
        .expect("ingest randomized corpus");
    store.flush().expect("flush randomized corpus");

    for query_index in 0..25_u64 {
        let mut attributes = Vec::new();
        if query_index % 2 == 0 {
            attributes.push((
                "region".to_owned(),
                Value::String(REGIONS[rng.below(REGIONS.len() as u64) as usize].to_owned()),
            ));
        }
        if query_index % 7 == 0 {
            attributes.push(("bucket".to_owned(), Value::from(rng.below(6))));
        }

        let since_ns = (query_index % 3 == 0).then(|| 1_000_000 + rng.below(1_600) * 1_000);
        let until_ns = (query_index % 4 == 0).then(|| {
            let lower = since_ns.unwrap_or(1_000_000);
            lower + rng.below(500_000)
        });
        let filter = SpanFilter {
            service: (query_index % 2 == 1)
                .then(|| SERVICES[rng.below(SERVICES.len() as u64) as usize].to_owned()),
            name: (query_index % 3 == 1)
                .then(|| NAMES[rng.below(NAMES.len() as u64) as usize].to_owned()),
            attributes,
            min_duration_ns: (query_index % 2 == 0).then(|| 1 + rng.below(20_000)),
            since_ns,
            until_ns,
            session: None,
            limit: (query_index % 5 == 0).then(|| 1 + rng.below(75) as usize),
            ..SpanFilter::default()
        };

        let expected = naive_query_ids(&spans, &filter);
        let actual = queried_ids(&store, &filter);
        assert_eq!(
            actual, expected,
            "query {query_index} differed from the independent reference: {filter:?}"
        );
    }
}

#[test]
fn ttl_compaction_drops_expired_segments() {
    let dir = TestDir::new("ttl-compaction");
    let store = Store::open(
        dir.path(),
        Config {
            flush_spans: 4,
            ttl_seconds: Some(1),
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            flush_wal_bytes: None,
        },
    )
    .expect("open store");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos() as u64;

    let expired_ids: Vec<_> = (0..8).map(|i| format!("expired-{i:02}")).collect();
    for (index, span_id) in expired_ids.iter().enumerate() {
        store
            .ingest(span(
                "expired-trace",
                span_id.clone(),
                now_ns - 10_000_000_000 + index as u64,
                100,
            ))
            .expect("ingest expired span");
    }
    store.flush().expect("flush expired segments");

    let fresh_ids: Vec<_> = (0..8).map(|i| format!("fresh-{i:02}")).collect();
    for (index, span_id) in fresh_ids.iter().enumerate() {
        store
            .ingest(span(
                "fresh-trace",
                span_id.clone(),
                now_ns + 1_000_000 + index as u64,
                100,
            ))
            .expect("ingest fresh span");
    }
    store.flush().expect("flush fresh segments");

    let segments_before = store
        .stats()
        .expect("stats before expiration")
        .segment_count;
    let removed = store
        .expire_before(now_ns)
        .expect("expire old segments at explicit cutoff");
    let segments_after = store.stats().expect("stats after expiration").segment_count;
    assert!(removed > 0, "expiration should remove stored data");
    assert!(
        segments_after < segments_before,
        "segment count should decrease after compaction"
    );

    let remaining: HashSet<_> = store
        .query(&SpanFilter::default())
        .expect("query compacted store")
        .into_iter()
        .map(|span| span.span_id)
        .collect();
    for span_id in &expired_ids {
        assert!(
            !remaining.contains(span_id),
            "expired span remains: {span_id}"
        );
    }
    for span_id in &fresh_ids {
        assert!(remaining.contains(span_id), "fresh span missing: {span_id}");
    }
    assert_eq!(remaining.len(), fresh_ids.len());
}

static CORRECTNESS_TEST_DIR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn correctness_test_dir(label: &str) -> std::path::PathBuf {
    let id = CORRECTNESS_TEST_DIR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("traza-{label}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create correctness test directory");
    path
}

fn correctness_span(batch: u64, item: u64) -> Span {
    Span {
        trace_id: format!("trace-{batch}"),
        span_id: format!("batch-{batch}-span-{item}"),
        parent_span_id: None,
        name: "correctness".to_string(),
        start_time_ns: batch * 1_000 + item,
        end_time_ns: batch * 1_000 + item + 10,
        status: Default::default(),
        service: "correctness-tests".to_string(),
        attributes: serde_json::Map::new(),
        events: Vec::new(),
        links: Vec::new(),
        extra: serde_json::Map::new(),
    }
}

#[test]
fn lock_order_no_deadlock() {
    let dir = correctness_test_dir("lock-order");
    let store = std::sync::Arc::new(
        Store::open(
            &dir,
            Config {
                flush_spans: 10_000,
                ttl_seconds: None,
                payload_threshold: None,
                durability: traza::Durability::Buffered,
                compaction: None,
                wal_commit_window: None,
                flush_wal_bytes: None,
            },
        )
        .expect("open store"),
    );
    store
        .ingest(correctness_span(0, 0))
        .expect("seed writer buffer");

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let flush_store = std::sync::Arc::clone(&store);
    let flush_done = done_tx.clone();
    let flusher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            flush_store.flush().expect("flush concurrently");
        }
        flush_done.send(()).expect("signal flusher completion");
    });

    let stats_store = std::sync::Arc::clone(&store);
    let stats_done = done_tx.clone();
    let statistician = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            stats_store.stats().expect("read stats concurrently");
        }
        stats_done.send(()).expect("signal statistician completion");
    });
    drop(done_tx);

    for _ in 0..2 {
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("flush/stats deadlocked");
    }
    flusher.join().expect("flusher thread");
    statistician.join().expect("statistician thread");
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove test directory");
}

#[test]
fn reads_never_miss_committed_spans() {
    const BATCHES: u64 = 100;
    const SPANS_PER_BATCH: u64 = 4;

    let dir = correctness_test_dir("atomic-reads");
    let store = std::sync::Arc::new(
        Store::open(
            &dir,
            Config {
                flush_spans: 10_000,
                ttl_seconds: None,
                payload_threshold: None,
                durability: traza::Durability::Buffered,
                compaction: None,
                wal_commit_window: None,
                flush_wal_bytes: None,
            },
        )
        .expect("open store"),
    );
    let watermark = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let writer_store = std::sync::Arc::clone(&store);
    let writer_watermark = std::sync::Arc::clone(&watermark);
    let writer = std::thread::spawn(move || {
        for batch in 1..=BATCHES {
            let spans = (0..SPANS_PER_BATCH)
                .map(|item| correctness_span(batch, item))
                .collect();
            writer_store.ingest_batch(spans).expect("ingest batch");
            writer_store.flush().expect("commit batch");
            writer_watermark.store(batch, std::sync::atomic::Ordering::Release);
        }
    });

    let reader_store = std::sync::Arc::clone(&store);
    let reader_watermark = std::sync::Arc::clone(&watermark);
    let reader = std::thread::spawn(move || loop {
        let committed = reader_watermark.load(std::sync::atomic::Ordering::Acquire);
        let spans = reader_store
            .query(&SpanFilter::default())
            .expect("query atomic snapshot");
        // Exactly-once, not merely present: a non-atomic snapshot's failure
        // mode is DUPLICATION (buffer copied, then flush moves the same spans
        // into segments before they are read again).
        let raw_count = spans.len();
        let ids: std::collections::HashSet<_> =
            spans.into_iter().map(|span| span.span_id).collect();
        assert_eq!(
            ids.len(),
            raw_count,
            "query returned duplicate spans: snapshot is not atomic"
        );
        for batch in 1..=committed {
            for item in 0..SPANS_PER_BATCH {
                assert!(
                    ids.contains(&format!("batch-{batch}-span-{item}")),
                    "query omitted committed batch {batch}, span {item}"
                );
            }
        }
        if committed == BATCHES {
            break;
        }
        std::thread::yield_now();
    });

    writer.join().expect("writer thread");
    reader.join().expect("reader thread");
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove test directory");
}

#[test]
fn stale_temp_does_not_wedge_flush() {
    let dir = correctness_test_dir("stale-temp");
    let orphan = dir.join(format!(".segment-0.json.{}.0.tmp", std::process::id()));
    std::fs::write(&orphan, b"orphaned partial segment").expect("plant stale temp");

    let store = Store::open(
        &dir,
        Config {
            flush_spans: 2,
            ttl_seconds: None,
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            flush_wal_bytes: None,
        },
    )
    .expect("recover store with stale temp");
    assert!(!orphan.exists(), "open did not remove stale temp");
    store
        .ingest(correctness_span(1, 0))
        .expect("ingest first span");
    store
        .ingest(correctness_span(1, 1))
        .expect("ingest second span");
    store
        .ingest(correctness_span(1, 2))
        .expect("ingest past threshold");
    store.flush().expect("flush after stale-temp recovery");

    drop(store);
    std::fs::remove_dir_all(dir).expect("remove test directory");
}

#[test]
fn stale_lock_from_dead_process_is_recovered() {
    // A SIGKILLed owner leaves its LOCK behind; the store must reclaim it
    // once the recorded PID is verifiably gone, or one crash wedges the
    // directory forever.
    let dir = correctness_test_dir("stale-lock");
    let dead_pid = {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawns");
        let pid = child.id();
        child.wait().expect("waits");
        pid
    };
    std::fs::write(
        dir.join("LOCK"),
        format!(
            "{dead_pid}
"
        ),
    )
    .expect("stale lock writes");
    let store = Store::open(&dir, Config::default())
        .expect("stale lock from a dead process must be reclaimed");
    drop(store);
}

#[test]
fn second_open_is_rejected() {
    let dir = correctness_test_dir("single-writer");
    let config = Config {
        flush_spans: 100,
        ttl_seconds: None,
        payload_threshold: None,
        durability: traza::Durability::Buffered,
        compaction: None,
        wal_commit_window: None,
        flush_wal_bytes: None,
    };
    let first = Store::open(&dir, config.clone()).expect("open first store");
    let second = Store::open(&dir, config.clone());
    assert!(matches!(second, Err(Error::AlreadyOpen)));

    drop(first);
    let reopened = Store::open(&dir, config).expect("reopen after first store drops");
    drop(reopened);
    std::fs::remove_dir_all(dir).expect("remove test directory");
}

#[test]
fn stale_lock_reclaim_has_exactly_one_winner() {
    // Review finding: remove-then-create reclamation let a slow reclaimer
    // delete the fast reclaimer's fresh lock. Racing openers on a stale lock
    // must produce exactly one owner; the rest see AlreadyOpen.
    let dir = correctness_test_dir("reclaim-race");
    let dead_pid = {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawns");
        let pid = child.id();
        child.wait().expect("waits");
        pid
    };
    std::fs::write(dir.join("LOCK"), format!("{dead_pid}\n")).expect("stale lock writes");

    let winners = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let dir = dir.clone();
        let winners = std::sync::Arc::clone(&winners);
        handles.push(std::thread::spawn(move || {
            if let Ok(store) = Store::open(&dir, Config::default()) {
                winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Hold the store long enough that late reclaimers race a LIVE
                // lock, not a released one.
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(store);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("thread joins");
    }
    assert_eq!(
        winners.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one racer may reclaim a stale lock"
    );
}

#[test]
fn ttl_zero_disables_expiration() {
    // Documented: zero disables TTL. Review finding: Some(0) computed a
    // cutoff of "now" and expired every existing span.
    let dir = correctness_test_dir("ttl-zero");
    let store = Store::open(
        &dir,
        Config {
            ttl_seconds: Some(0),
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");
    store
        .ingest(span("t-zero", "s1".to_owned(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("flush");
    let removed = store.compact_expired().expect("compact");
    assert_eq!(removed, 0, "ttl 0 must be a no-op");
    assert_eq!(
        store.get_trace("t-zero").expect("read").len(),
        1,
        "spans must survive a ttl-0 compaction"
    );
}

#[test]
fn supersede_journal_finishes_interrupted_rewrite() {
    // Crash between the rewritten segment's rename and the original's delete:
    // the journal marker lets recovery finish the delete — no content
    // guessing involved.
    let dir = correctness_test_dir("supersede-finish");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(span("t-sup", "s1".to_owned(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("flush");
    drop(store);

    let original = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("segment-"))
        })
        .expect("segment exists");
    let old_name = original.file_name().unwrap().to_string_lossy().into_owned();
    let new_name = "segment-00000000000000000099.seg";
    let replacement = original.with_file_name(new_name);
    std::fs::copy(&original, &replacement).expect("replacement in place");
    std::fs::write(
        dir.join(format!(".supersede.{old_name}.{new_name}.journal")),
        format!("{old_name} -> {new_name}\n"),
    )
    .expect("marker");

    let store = Store::open(&dir, Config::default()).expect("recovers");
    assert!(
        !original.exists(),
        "journal recovery must delete the superseded original"
    );
    assert_eq!(
        store.get_trace("t-sup").expect("trace").len(),
        1,
        "exactly one copy served after recovery"
    );
}

#[test]
fn supersede_journal_without_replacement_keeps_original() {
    // Crash before the replacement materialized: the original stays
    // authoritative and the stale marker is cleared.
    let dir = correctness_test_dir("supersede-abort");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(span("t-abort", "s1".to_owned(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("flush");
    drop(store);

    let original = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("segment-"))
        })
        .expect("segment exists");
    let old_name = original.file_name().unwrap().to_string_lossy().into_owned();
    let marker = dir.join(format!(
        ".supersede.{old_name}.segment-00000000000000000099.seg.journal"
    ));
    std::fs::write(
        &marker,
        format!("{old_name} -> segment-00000000000000000099.seg\n"),
    )
    .expect("marker");

    let store = Store::open(&dir, Config::default()).expect("recovers");
    assert!(
        original.exists(),
        "original must survive an aborted rewrite"
    );
    assert!(!marker.exists(), "stale marker must be cleared");
    assert_eq!(store.get_trace("t-abort").expect("trace").len(), 1);
}

#[test]
fn span_identity_is_a_primary_key() {
    // (trace_id, span_id) is enforced unique: re-ingesting the key replaces
    // the span (retries are idempotent), in the buffer, across flushes, and
    // across restart. Last write wins.
    let dir = correctness_test_dir("primary-key");
    let store = Store::open(&dir, Config::default()).expect("opens");

    // Buffer-level replace.
    let mut first = span("t-pk", "same-span".to_owned(), 1_000, 10);
    first.status = "first".to_owned();
    store.ingest(first).expect("first ingest");
    let mut second = span("t-pk", "same-span".to_owned(), 1_000, 10);
    second.status = "second".to_owned();
    store.ingest(second).expect("retry ingest replaces");
    assert_eq!(store.get_trace("t-pk").expect("trace").len(), 1);
    store.flush().expect("flush");

    // Cross-segment replace: same key re-ingested after a flush.
    let mut third = span("t-pk", "same-span".to_owned(), 1_000, 10);
    third.status = "third".to_owned();
    store.ingest(third).expect("post-flush re-ingest");
    store.flush().expect("second flush");
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens");
    let spans = store.get_trace("t-pk").expect("trace");
    assert_eq!(
        spans.len(),
        1,
        "one span per key, across segments and restart"
    );
    assert_eq!(spans[0].status, "third", "last write wins");
    let filtered = store
        .query(&SpanFilter {
            service: Some("test-service".into()),
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(filtered.len(), 1, "queries also see exactly one version");
    assert_eq!(filtered[0].status, "third");
}

#[test]
fn full_key_cursor_pages_equal_timestamps_with_a_constant_limit() {
    let dir = correctness_test_dir("full-key-cursor");
    let store = Store::open(&dir, Config::default()).expect("opens");
    for index in 0..5_000 {
        store
            .ingest(span(
                &format!("trace-{index:05}"),
                "span".to_owned(),
                42_000,
                1_000,
            ))
            .expect("ingests");
        if index == 2_499 {
            store.flush().expect("flushes first half");
        }
    }
    store.flush().expect("flushes second half");

    let filter = SpanFilter {
        service: Some("test-service".into()),
        limit: Some(257),
        ..SpanFilter::default()
    };
    let mut cursor: Option<SpanCursor> = None;
    let mut ids = Vec::new();
    loop {
        let page = store
            .query_after(&filter, cursor.as_ref())
            .expect("queries page");
        assert!(page.len() <= 257, "page bound is invariant");
        if page.is_empty() {
            break;
        }
        ids.extend(page.iter().map(|item| item.trace_id.clone()));
        cursor = page.last().map(SpanCursor::from);
    }
    assert_eq!(ids.len(), 5_000);
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "cursor preserves the complete total order"
    );
}

#[test]
fn stats_name_physical_records_explicitly() {
    let dir = correctness_test_dir("physical-stats");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(span("trace", "span".into(), 1_000, 10))
        .expect("v1");
    store.flush().expect("flushes v1");
    store
        .ingest(span("trace", "span".into(), 2_000, 10))
        .expect("v2");
    store.flush().expect("flushes v2");

    assert_eq!(store.query(&SpanFilter::default()).unwrap().len(), 1);
    let stats = store.stats().expect("stats");
    assert_eq!(stats.persisted_records, 2);
    assert_eq!(stats.total_records, 2);
    assert_eq!(stats.buffered_records, 0);
}

#[test]
fn empty_stale_sentinel_does_not_wedge_reclaim() {
    // A reclaimer that died before writing its PID leaves an empty sentinel.
    // Backdated past the age threshold, it must not block recovery forever.
    let dir = correctness_test_dir("sentinel-wedge");
    let dead_pid = {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawns");
        let pid = child.id();
        child.wait().expect("waits");
        pid
    };
    std::fs::write(dir.join("LOCK"), format!("{dead_pid}\n")).expect("stale lock");
    let sentinel = dir.join("LOCK.reclaim");
    std::fs::write(&sentinel, "").expect("empty sentinel");
    // Backdate the sentinel beyond the 10s age threshold.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&sentinel)
        .expect("open sentinel");
    file.set_modified(old).expect("backdate");
    drop(file);

    // First open clears the corpse sentinel; a retry wins the reclaim.
    let first = Store::open(&dir, Config::default());
    let second = Store::open(&dir, Config::default());
    assert!(
        first.is_ok() || second.is_ok(),
        "an aged empty sentinel must not wedge recovery"
    );
}

#[test]
fn v2_open_holds_no_resident_span_structs() {
    // The v2 memory rule: after reopen, persisted data lives as bytes plus
    // indexes — zero materialized Span structs — and reads still work.
    let dir = correctness_test_dir("v2-residency");
    let store = Store::open(&dir, Config::default()).expect("opens");
    for i in 0..50 {
        store
            .ingest(span("t-res", format!("s{i}"), 1_000 + i, 10))
            .expect("ingest");
    }
    store.flush().expect("flush");
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens");
    assert_eq!(
        store
            .resident_persisted_span_structs()
            .expect("resident count"),
        0,
        "v2 segments must not materialize spans at open"
    );
    assert_eq!(store.get_trace("t-res").expect("trace").len(), 50);
    let filtered = store
        .query(&SpanFilter {
            service: Some("test-service".into()),
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(
        filtered.len(),
        50,
        "index-served query must return all spans"
    );
}

#[test]
fn legacy_v1_segment_fails_loudly() {
    // v1 JSONL segments are no longer supported: opening a directory that
    // contains one must fail with a clear error, never silently hide data.
    let dir = correctness_test_dir("v1-refused");
    std::fs::write(
        dir.join("segment-00000000000000000000.jsonl"),
        "{\"not\":\"supported\"}\n",
    )
    .expect("writes legacy file");
    let result = Store::open(&dir, Config::default());
    assert!(result.is_err(), "legacy segments must be refused loudly");
    let message = format!("{}", result.err().expect("error"));
    assert!(
        message.contains("migrate"),
        "error must say how to migrate: {message}"
    );
}

#[test]
fn flush_after_reopen_does_not_overwrite_segments() {
    // Found in review, reproduced across restart: next-id computation only
    // recognized .jsonl names, so a reopened v2-only store restarted
    // numbering at zero and the next flush renamed over segment 0 —
    // destroying persisted spans.
    let dir = correctness_test_dir("reopen-no-overwrite");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(span("t-first", "s1".to_owned(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("flush");
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens");
    store
        .ingest(span("t-second", "s2".to_owned(), 2_000, 10))
        .expect("ingest after reopen");
    store.flush().expect("flush after reopen");
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens again");
    assert_eq!(
        store.get_trace("t-first").expect("first").len(),
        1,
        "pre-restart span survives"
    );
    assert_eq!(store.get_trace("t-second").expect("second").len(), 1);
    assert_eq!(store.stats().expect("stats").segment_count, 2);
}

#[test]
fn corrupt_segment_header_is_an_error_not_a_panic() {
    let dir = correctness_test_dir("corrupt-header");
    // A file with the RIGHT magic (so it reaches the header parse) but a
    // nonsense attribute-index offset, which used to panic through unsigned
    // subtraction (found in review). The magic must stay current, or this
    // short-circuits at the magic check and no longer tests the overflow.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TRAZASEG");
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&80u16.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&80u64.to_le_bytes());
    bytes.extend_from_slice(&8u64.to_le_bytes());
    bytes.extend_from_slice(&88u64.to_le_bytes());
    bytes.extend_from_slice(&8u64.to_le_bytes());
    bytes.extend_from_slice(&96u64.to_le_bytes());
    bytes.extend_from_slice(&8u64.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // absurd attr offset
    bytes.resize(200, 0);
    std::fs::write(dir.join("segment-00000000000000000000.seg"), &bytes).expect("writes");
    let result = Store::open(&dir, Config::default());
    assert!(result.is_err(), "corrupt header must surface as an error");
}

#[test]
fn nul_prefixed_attribute_cannot_poison_the_service_index() {
    let dir = correctness_test_dir("index-poison");
    let store = Store::open(&dir, Config::default()).expect("opens");
    let mut poisoned = span("t-poison", "s1".to_owned(), 1_000, 10);
    poisoned.attributes.insert(
        "\u{0}service".to_owned(),
        serde_json::Value::String("evil".to_owned()),
    );
    store.ingest(poisoned).expect("ingest");
    store.flush().expect("flush");
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens");
    let by_service = store
        .query(&SpanFilter {
            service: Some("test-service".into()),
            ..SpanFilter::default()
        })
        .expect("service query");
    assert_eq!(
        by_service.len(),
        1,
        "real service query must not be poisoned"
    );
    // The hostile attribute is still stored verbatim and queryable via the
    // (index-declining) scan path.
    let by_attr = store
        .query(&SpanFilter {
            attributes: vec![(
                "\u{0}service".to_owned(),
                serde_json::Value::String("evil".to_owned()),
            )],
            ..SpanFilter::default()
        })
        .expect("hostile attr query");
    assert_eq!(by_attr.len(), 1, "the attribute itself remains queryable");
}

#[test]
fn file_backed_segments_hold_no_resident_payload() {
    // Leg 1, larger-than-RAM: segments hold parsed indexes only — payload
    // bytes are read on demand from the file. Zero resident payload after
    // flush AND after reopen, with reads still exact.
    let dir = correctness_test_dir("file-backed-residency");
    let store = Store::open(&dir, Config::default()).expect("opens");
    for i in 0..200 {
        store
            .ingest(span("t-fb", format!("s{i}"), 1_000 + i, 10))
            .expect("ingest");
    }
    store.flush().expect("flush");
    assert_eq!(
        store.resident_payload_bytes().expect("resident"),
        0,
        "flushing must not leave a resident payload copy"
    );
    drop(store);

    let store = Store::open(&dir, Config::default()).expect("reopens");
    assert_eq!(store.resident_payload_bytes().expect("resident"), 0);
    assert_eq!(store.resident_persisted_span_structs().expect("structs"), 0);
    assert_eq!(store.get_trace("t-fb").expect("trace").len(), 200);
    let filtered = store
        .query(&SpanFilter {
            service: Some("test-service".into()),
            limit: Some(50),
            ..SpanFilter::default()
        })
        .expect("limited filter");
    assert_eq!(filtered.len(), 50, "index-served reads work file-backed");
}

/// A store whose acknowledgements are backed by the log, with a threshold high
/// enough that nothing seals unless the test asks for it.
fn wal_config(flush_spans: usize) -> Config {
    Config {
        flush_spans,
        durability: traza::Durability::Wal,
        compaction: None,
        ..Config::default()
    }
}

#[test]
fn ttl_expiry_reaches_the_write_ahead_log() {
    // Found in review: expiry removed spans from the write buffer and left the
    // log records that carried them in place, so a restart replayed the
    // expired span and it came back. Retention a restart undoes is not
    // retention, and telemetry deleted on request must not be recoverable
    // from the log.
    let dir = correctness_test_dir("ttl-wal");
    let store = Store::open(&dir, wal_config(100_000)).expect("opens");
    store
        .ingest(span("t-old", "expired".into(), 1_000, 10))
        .expect("acknowledged in wal mode");
    store
        .ingest(span("t-new", "survivor".into(), 9_000, 10))
        .expect("acknowledged in wal mode");
    let before = store.stats().expect("stats");
    assert_eq!(before.buffered_records, 2);
    assert!(before.wal_bytes > 0, "both spans are in the log");

    assert_eq!(store.expire_before(5_000).expect("expires"), 1);
    let after = store.stats().expect("stats");
    assert_eq!(after.buffered_records, 1);
    assert!(
        after.wal_bytes > 0 && after.wal_bytes < before.wal_bytes,
        "the log is rewritten to the survivors, not left holding the expired \
         span: {before:?} -> {after:?}"
    );
    drop(store);

    let reopened = Store::open(&dir, wal_config(100_000)).expect("reopens");
    let ids = queried_ids(&reopened, &SpanFilter::default());
    assert_eq!(
        ids,
        vec!["survivor".to_owned()],
        "the expired span must not come back across a restart"
    );
    assert_eq!(reopened.stats().expect("stats").buffered_records, 1);
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn expiring_everything_empties_the_log() {
    let dir = correctness_test_dir("ttl-wal-empty");
    let store = Store::open(&dir, wal_config(100_000)).expect("opens");
    store
        .ingest(span("t-old", "expired".into(), 1_000, 10))
        .expect("ingest");
    assert_eq!(store.expire_before(5_000).expect("expires"), 1);
    assert_eq!(
        store.stats().expect("stats").wal_bytes,
        0,
        "nothing survived, so nothing is left to replay"
    );
    drop(store);

    let reopened = Store::open(&dir, wal_config(100_000)).expect("reopens");
    assert!(queried_ids(&reopened, &SpanFilter::default()).is_empty());
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_damaged_interior_log_frame_refuses_to_open() {
    // Found in review: replay stopped at the first bad frame and reported
    // success, so three acknowledged batches with damage in the second came
    // back as one — the third vanished without a word. Interior damage is not
    // a torn tail and must not be resolved by guessing.
    let dir = correctness_test_dir("wal-interior");
    let store = Store::open(&dir, wal_config(100_000)).expect("opens");
    for (index, id) in ["one", "two", "three"].iter().enumerate() {
        store
            .ingest(span("t", (*id).to_owned(), 1_000 + index as u64, 10))
            .expect("acknowledged");
    }
    drop(store);

    // Corrupt a payload byte in the SECOND of the three frames. Every byte the
    // frame declared is present, so this cannot be an interrupted append.
    let log = dir.join("wal.log");
    let mut bytes = fs::read(&log).expect("read log");
    let first_frame = 8 + u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let target = first_frame + 10;
    bytes[target] ^= 0xFF;
    fs::write(&log, &bytes).expect("write log");

    match Store::open(&dir, wal_config(100_000)) {
        Err(Error::WalCorrupt(message)) => {
            assert!(
                message.contains("wal.log"),
                "the operator is told which file: {message}"
            );
        }
        Ok(_) => panic!("opening must not silently drop the batches after the damage"),
        Err(other) => panic!("expected a corruption error, got {other}"),
    }

    // Moving the log aside is the deliberate, lossy escape hatch.
    fs::rename(&log, dir.join("wal.log.quarantine")).expect("quarantine");
    let store = Store::open(&dir, wal_config(100_000)).expect("opens without the log");
    assert!(queried_ids(&store, &SpanFilter::default()).is_empty());
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn repeated_updates_to_one_key_still_reach_a_flush() {
    // Found in review: the flush threshold counted unique buffered records, so
    // a workload updating one key never reached it — the buffer stayed at one
    // record while the log grew with every acknowledged write, without bound.
    let dir = correctness_test_dir("hot-key");
    let store = Store::open(&dir, wal_config(8)).expect("opens");
    for round in 0..500u64 {
        let mut hot = span("t-hot", "hot".into(), 1_000, 10);
        hot.attributes
            .insert("round".to_owned(), Value::from(round));
        store.ingest(hot).expect("acknowledged");
    }

    let stats = store.stats().expect("stats");
    assert!(
        stats.segment_count > 0,
        "updates are work and must seal: {stats:?}"
    );
    assert!(
        stats.wal_bytes < 8_000,
        "the log holds at most a threshold's worth of updates: {stats:?}"
    );

    // Bounding the log must not cost the answer.
    let spans = store.get_trace("t-hot").expect("trace");
    assert_eq!(spans.len(), 1, "one key, one span: {spans:?}");
    assert_eq!(spans[0].attributes["round"], Value::from(499u64));
    drop(store);

    let reopened = Store::open(&dir, wal_config(8)).expect("reopens");
    let spans = reopened.get_trace("t-hot").expect("trace");
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].attributes["round"],
        Value::from(499u64),
        "the newest version survives the seal and the restart"
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_log_byte_bound_seals_even_without_the_record_bound() {
    let dir = correctness_test_dir("wal-bytes");
    let store = Store::open(
        &dir,
        Config {
            // Unreachable record bounds: the byte bound is the only one left.
            flush_spans: 1_000_000,
            flush_wal_bytes: Some(16 * 1024),
            durability: traza::Durability::Wal,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");
    for index in 0..400u64 {
        store
            .ingest(span("t-bytes", format!("s{index}"), 1_000 + index, 10))
            .expect("acknowledged");
    }
    let stats = store.stats().expect("stats");
    assert!(
        stats.segment_count > 0 && stats.wal_bytes < 32 * 1024,
        "the byte bound seals on its own: {stats:?}"
    );
    assert_eq!(
        store.get_trace("t-bytes").expect("trace").len(),
        400,
        "every acknowledged span is still readable"
    );
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_pinned_snapshot_is_one_dataset_while_the_store_changes() {
    // Found in review: export paged the LIVE store, so a span re-ingested
    // behind the cursor was emitted a second time and the result held two
    // versions of one primary key while the trailer said "complete".
    let dir = correctness_test_dir("snapshot");
    let store = Store::open(&dir, Config::default()).expect("opens");
    for index in 0..500u64 {
        store
            .ingest(span("t-export", format!("s{index:03}"), 1_000 + index, 10))
            .expect("ingest");
    }
    store.flush().expect("flush");

    let view = store.snapshot().expect("pins");

    // Everything an export can race with: a replaced span, a new span, a seal,
    // and expiry deleting a pinned segment out from under the view.
    let mut replaced = span("t-export", "s000".into(), 1_000, 10);
    replaced
        .attributes
        .insert("version".to_owned(), Value::from("second"));
    store.ingest(replaced).expect("re-ingest");
    store
        .ingest(span("t-export", "s999".into(), 9_999, 10))
        .expect("ingest");
    store.flush().expect("flush");
    store.expire_before(1_200).expect("expires");

    // Page the view exactly as export does.
    let mut cursor: Option<SpanCursor> = None;
    let mut seen: Vec<Span> = Vec::new();
    loop {
        let page = view
            .query_after(
                &SpanFilter {
                    limit: Some(64),
                    ..SpanFilter::default()
                },
                cursor.as_ref(),
            )
            .expect("page");
        if page.is_empty() {
            break;
        }
        cursor = Some(SpanCursor::from(page.last().expect("last")));
        seen.extend(page);
    }

    assert_eq!(seen.len(), 500, "the view is the dataset it pinned");
    let unique: HashSet<(String, String)> = seen
        .iter()
        .map(|span| (span.trace_id.clone(), span.span_id.clone()))
        .collect();
    assert_eq!(unique.len(), 500, "no primary key appears twice");
    let first = seen
        .iter()
        .find(|span| span.span_id == "s000")
        .expect("s000");
    assert!(
        !first.attributes.contains_key("version"),
        "the view holds the version that existed when it was pinned"
    );
    assert!(
        !seen.iter().any(|span| span.span_id == "s999"),
        "a span ingested after the pin is not part of the dataset"
    );

    // The live store meanwhile reflects every one of those changes.
    let live = queried_ids(&store, &SpanFilter::default());
    assert!(live.contains(&"s999".to_owned()));
    assert!(
        !live.contains(&"s000".to_owned()),
        "expiry removed the earliest spans from the live store"
    );
    drop(view);
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_expiry_rewrite_keeps_its_place_in_recency_order() {
    // Segment path order IS recency order, so a rewrite that took a fresh
    // (highest) id moved an old segment to the newest position — and its stale
    // version of a re-ingested span then won. Found while making expiry reach
    // the log: the survivors are renamed onto the same name now.
    let dir = correctness_test_dir("expiry-order");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");

    // Older segment: the shared key at v1, plus a span that will expire.
    let mut first = span("t-order", "shared".into(), 8_000, 10);
    first
        .attributes
        .insert("version".to_owned(), Value::from("first"));
    store.ingest(first).expect("ingest");
    store
        .ingest(span("t-order", "doomed".into(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("seals the older segment");

    // Newer segment: the shared key at v2. Nothing here expires, so only the
    // older segment is rewritten.
    let mut second = span("t-order", "shared".into(), 8_000, 10);
    second
        .attributes
        .insert("version".to_owned(), Value::from("second"));
    store.ingest(second).expect("ingest");
    store.flush().expect("seals the newer segment");

    assert_eq!(store.expire_before(5_000).expect("expires"), 1);

    let shared = store
        .get_trace("t-order")
        .expect("trace")
        .into_iter()
        .find(|span| span.span_id == "shared")
        .expect("shared span");
    assert_eq!(
        shared.attributes["version"],
        Value::from("second"),
        "rewriting the older segment must not promote it past the newer one"
    );
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

/// True when this process can write through a read-only directory anyway,
/// which is how the permission-based fault injection below stops being a
/// fault. Root is the usual case (containers, some CI).
fn ignores_directory_permissions() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn a_failed_log_rewrite_leaves_expiry_retryable() {
    // Found in review: expiry dropped the span from the write buffer BEFORE
    // the log rewrite succeeded. A failed rewrite therefore left memory ahead
    // of the recovery authority — and the retry saw nothing left to expire,
    // returned Ok(0), and never repaired the log, so the restart resurrected
    // the span. Durable first, then the infallible in-memory swap.
    let dir = correctness_test_dir("expiry-wal-failure");
    let store = Store::open(&dir, wal_config(100_000)).expect("opens");
    store
        .ingest(span("t-old", "expired".into(), 1_000, 10))
        .expect("acknowledged");
    store
        .ingest(span("t-new", "survivor".into(), 9_000, 10))
        .expect("acknowledged");
    let before = store.stats().expect("stats");

    // Occupy the rewrite's staging path with a directory: the staging open
    // then fails deterministically, whatever the platform or the uid.
    let blocker = dir.join(".wal.log.rewrite.tmp");
    fs::create_dir(&blocker).expect("blocks staging");

    let failed = store.expire_before(5_000);
    assert!(
        failed.is_err(),
        "the log rewrite could not be staged, so expiry must fail: {failed:?}"
    );
    let after_failure = store.stats().expect("stats");
    assert_eq!(
        after_failure.buffered_records, 2,
        "a failed expiry must leave memory and the log agreeing, not drop the \
         span from memory alone"
    );
    assert_eq!(
        after_failure.wal_bytes, before.wal_bytes,
        "and it must not have touched the log either"
    );

    // The retry is what repairs it, which only works if the first attempt left
    // something to retry.
    fs::remove_dir(&blocker).expect("unblocks staging");
    assert_eq!(
        store.expire_before(5_000).expect("retry"),
        1,
        "the retry still has the expired span to remove"
    );
    assert!(store.stats().expect("stats").wal_bytes < before.wal_bytes);
    drop(store);

    let reopened = Store::open(&dir, wal_config(100_000)).expect("reopens");
    assert_eq!(
        queried_ids(&reopened, &SpanFilter::default()),
        vec!["survivor".to_owned()],
        "the expired span must not survive the failure-then-retry path"
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_segment_unlink_leaves_expiry_retryable() {
    // The same defect on the segment path: a fully expired segment was removed
    // from the live list before its file was unlinked, so a failed unlink left
    // the store reporting zero segments over a file that was still there — and
    // the retry, seeing no segments, returned Ok(0). The restart loaded the
    // segment and resurrected its spans.
    if ignores_directory_permissions() {
        eprintln!("skipped: this process writes through read-only directories");
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let dir = correctness_test_dir("expiry-unlink-failure");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");
    store
        .ingest(span("t-old", "expired".into(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("seals a fully expirable segment");
    assert_eq!(store.stats().expect("stats").segment_count, 1);

    // Unlinking needs write permission on the directory, not on the file.
    let readonly = fs::Permissions::from_mode(0o555);
    let writable = fs::Permissions::from_mode(0o755);
    fs::set_permissions(&dir, readonly).expect("read-only");

    let failed = store.expire_before(5_000);
    assert!(
        failed.is_err(),
        "the segment file could not be unlinked, so expiry must fail: {failed:?}"
    );
    assert_eq!(
        store.stats().expect("stats").segment_count,
        1,
        "a segment that is still on disk must still be in the live list"
    );

    fs::set_permissions(&dir, writable).expect("writable");
    assert_eq!(
        store.expire_before(5_000).expect("retry"),
        1,
        "the retry still has the segment to remove"
    );
    assert_eq!(store.stats().expect("stats").segment_count, 0);
    drop(store);

    let reopened = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("reopens");
    assert!(
        queried_ids(&reopened, &SpanFilter::default()).is_empty(),
        "the expired segment must not come back"
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}
