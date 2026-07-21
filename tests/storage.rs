//! End-to-end storage tests covering persistence, recovery, filtering, and expiry.

use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use traza::{Config, Span, SpanFilter, Store};

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
