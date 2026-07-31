//! The buffer's non-volume bounds: a trickle store must seal on age, and a
//! store whose aggregations observe segment-versus-segment key shadowing must
//! deduplicate itself — the production shape this exists for was a deployment
//! writing ~150 spans/day against a 10,000-span threshold, whose buffered
//! upserts disqualified every segment's rollup for 36 days straight.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use traza::analytics::LlmGroupBy;
use traza::{Config, Span, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-selfseal-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A span whose identity and payload are both derived from `key`, so
/// re-ingesting the same key builds the upsert shapes the triggers exist for.
fn span(key: u64, version: u32, start_ns: u64) -> Span {
    serde_json::from_value(json!({
        "trace_id": format!("trace-{key}"),
        "span_id": format!("span-{key}"),
        "name": "op",
        "service": "self-seal-test",
        "start_time_ns": start_ns,
        "end_time_ns": start_ns + 1_000,
        "status": "ok",
        "attributes": { "version": version },
    }))
    .expect("span")
}

/// No volume threshold fires in these tests; whatever seals, a bound did.
fn config() -> Config {
    Config {
        flush_spans: 10_000,
        ..Config::default()
    }
}

const BASE_NS: u64 = 1_700_000_000_000_000_000;

fn aggregate_span_total(store: &Store) -> usize {
    store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregate")
        .iter()
        .map(|row| row.spans)
        .sum()
}

/// The live-deployment regression: two segments sharing upserted keys, plus a
/// buffered upsert. One aggregation observes the segment-versus-segment
/// shadowing; one maintenance pass must merge the shadowed run into a single
/// deduplicated segment. The buffered upsert is deliberately NOT sealed by
/// the pass — buffer-caused shadowing is the age bound's problem — and the
/// aggregation must be exact before and after.
#[test]
fn observed_shadowing_merges_the_shadowed_run() {
    let dir = test_dir("shadow");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: None,
            ..config()
        },
    )
    .expect("opens");

    // Segment A: keys 0..100.
    store
        .ingest_batch((0..100).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    store.flush().expect("flush");
    // Segment B: updated versions of keys 0..10, plus new keys 100..120.
    let mut second: Vec<Span> = (0..10).map(|key| span(key, 2, BASE_NS + key)).collect();
    second.extend((100..120).map(|key| span(key, 1, BASE_NS + key)));
    store.ingest_batch(second).expect("ingest");
    store.flush().expect("flush");
    // A third version of key 0 stays in the buffer.
    store.ingest(span(0, 3, BASE_NS)).expect("ingest");

    let before = store.stats().expect("stats");
    assert_eq!(before.segment_count, 2);
    assert_eq!(before.buffered_records, 1);
    assert_eq!(aggregate_span_total(&store), 120);

    store.maintain_buffer().expect("maintain");

    let after = store.stats().expect("stats");
    assert_eq!(
        after.segment_count, 1,
        "shadowed run merges into one deduplicated segment"
    );
    assert_eq!(
        after.persisted_records, 120,
        "superseded versions are merged away, not retained"
    );
    assert_eq!(
        after.buffered_records, 1,
        "the shadow pass merges; it does not seal the buffer"
    );
    assert_eq!(aggregate_span_total(&store), 120);
}

/// A successful merge cools the pass down: fresh shadowing observed right
/// afterwards is not answered with another rewrite.
#[test]
fn shadow_passes_cool_down_after_a_merge() {
    let dir = test_dir("cooldown");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: None,
            ..config()
        },
    )
    .expect("opens");

    store
        .ingest_batch((0..20).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    store.flush().expect("flush");
    store.ingest(span(3, 2, BASE_NS + 3)).expect("ingest");
    store.flush().expect("flush");

    store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregate");
    store.maintain_buffer().expect("maintain");
    assert_eq!(store.stats().expect("stats").segment_count, 1);

    // Re-poison: a new segment shadowing the merged one.
    store.ingest(span(4, 2, BASE_NS + 4)).expect("ingest");
    store.flush().expect("flush");
    store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregate");
    store.maintain_buffer().expect("maintain");
    assert_eq!(
        store.stats().expect("stats").segment_count,
        2,
        "a second merge within the cooldown is declined"
    );
    assert_eq!(aggregate_span_total(&store), 20);
}

/// Buffer-caused shadowing alone must not trigger the pass: a merge cannot
/// retire a key the client is still updating, so nothing may latch.
#[test]
fn buffer_shadowing_alone_triggers_nothing() {
    let dir = test_dir("buffer-shadow");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: None,
            ..config()
        },
    )
    .expect("opens");

    store
        .ingest_batch((0..30).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    store.flush().expect("flush");
    // The poisoned live state: a buffered upsert of a persisted key.
    store.ingest(span(7, 2, BASE_NS + 7)).expect("ingest");

    assert_eq!(aggregate_span_total(&store), 30);
    store.maintain_buffer().expect("maintain");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.segment_count, 1, "nothing merged");
    assert_eq!(stats.buffered_records, 1, "nothing sealed");
}

/// Fresh keys shadow nothing: an insert-only trickle must not trigger the
/// shadow pass, however many aggregations run.
#[test]
fn pure_inserts_never_trigger_shadow_maintenance() {
    let dir = test_dir("inserts");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: None,
            ..config()
        },
    )
    .expect("opens");

    store
        .ingest_batch((0..50).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    store.flush().expect("flush");
    store
        .ingest_batch((50..80).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    store.flush().expect("flush");
    store.ingest(span(80, 1, BASE_NS + 80)).expect("ingest");

    store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregate");
    store.maintain_buffer().expect("maintain");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.buffered_records, 1, "nothing sealed the fresh span");
    assert_eq!(stats.segment_count, 2, "nothing merged");
}

/// An idle buffer seals once its oldest span reaches the age bound, via the
/// maintenance path a scheduler drives.
#[test]
fn age_bound_seals_an_idle_buffer() {
    let dir = test_dir("age");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: Some(Duration::from_millis(50)),
            shadow_seal: false,
            ..config()
        },
    )
    .expect("opens");

    store
        .ingest_batch((0..3).map(|key| span(key, 1, BASE_NS + key)).collect())
        .expect("ingest");
    let stats = store.stats().expect("stats");
    assert_eq!(stats.buffered_records, 3);
    assert!(
        stats.buffer_age_seconds.is_some(),
        "a non-empty buffer reports its age"
    );

    // Younger than the bound: maintenance leaves it alone.
    store.maintain_buffer().expect("maintain");
    assert_eq!(store.stats().expect("stats").buffered_records, 3);

    std::thread::sleep(Duration::from_millis(80));
    store.maintain_buffer().expect("maintain");
    let stats = store.stats().expect("stats");
    assert_eq!(stats.buffered_records, 0, "age bound sealed the buffer");
    assert_eq!(stats.segment_count, 1);
    assert_eq!(stats.buffer_age_seconds, None, "an empty buffer has no age");
}

/// The age bound also gates on the ingest path, so an actively written store
/// seals on its next batch instead of waiting for a maintenance tick.
#[test]
fn age_bound_fires_on_the_ingest_path() {
    let dir = test_dir("age-ingest");
    let store = Store::open(
        &dir,
        Config {
            max_buffer_age: Some(Duration::from_millis(50)),
            shadow_seal: false,
            ..config()
        },
    )
    .expect("opens");

    store.ingest(span(0, 1, BASE_NS)).expect("ingest");
    std::thread::sleep(Duration::from_millis(80));
    store.ingest(span(1, 1, BASE_NS + 1)).expect("ingest");

    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.buffered_records, 0,
        "the batch that found the buffer over-age sealed it"
    );
    assert_eq!(stats.segment_count, 1);
    assert_eq!(stats.persisted_records, 2);
}
