//! End-to-end storage tests covering persistence, recovery, filtering, and expiry.

use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use traza::{Config, Error, Span, SpanFilter, Store};

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
            limit: (query_index % 5 == 0).then(|| 1 + rng.below(75) as usize),
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
fn recovery_heals_crash_duplicated_segments() {
    // Simulate a crash between writing the compacted replacement segment and
    // deleting its original: both files exist, holding the same surviving
    // span. Reopen must return ONE copy and heal the extra file.
    let dir = correctness_test_dir("dup-heal");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(span("t-dup", "s1".to_owned(), 1_000, 10))
        .expect("ingest");
    store.flush().expect("flush");
    drop(store);

    let segment_path = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                || path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("segment"))
        })
        .expect("one segment file exists");
    // A real crash duplicate is the compaction's REWRITTEN segment: a valid
    // next-numbered segment name the loader will pick up.
    let duplicate = segment_path.with_file_name("segment-00000000000000000099.jsonl");
    std::fs::copy(&segment_path, &duplicate).expect("simulate crash duplicate");

    let store = Store::open(&dir, Config::default()).expect("reopens");
    let spans = store.get_trace("t-dup").expect("read");
    assert_eq!(spans.len(), 1, "recovery must return one copy, not two");
    drop(store);
    assert!(
        !duplicate.exists(),
        "the fully-duplicate segment file must be healed away"
    );
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
