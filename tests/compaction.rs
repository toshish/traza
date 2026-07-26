//! Size-tiered compaction: it must bound segment count WITHOUT changing a
//! single answer.
//!
//! Merging rewrites the physical layout that last-write-wins depends on —
//! segment order is recency order — so every test here checks the data as
//! well as the segment count. A compaction that loses the newest version of a
//! re-ingested span would be far worse than the slow search it fixes.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use traza::{CompactionConfig, Config, Durability, Span, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-compact-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Compaction that triggers on tiny segments, so tests stay fast.
fn eager(fanout: usize) -> Config {
    Config {
        durability: Durability::Buffered,
        compaction: Some(CompactionConfig {
            fanout,
            base_bytes: 1,
            max_segment_bytes: 0,
        }),
        ..Config::default()
    }
}

fn span(trace: &str, id: &str, name: &str, start_ns: u64) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace, "span_id": id, "name": name, "service": "svc",
        "start_time_ns": start_ns, "end_time_ns": start_ns + 100,
        "attributes": {"group": "g1"}
    }))
    .expect("span")
}

/// Flushes one segment per call so segment count is controlled exactly.
fn seal(store: &Store, spans: Vec<Span>) {
    for span in spans {
        store.ingest(span).expect("ingest");
    }
    store.flush().expect("flush");
}

#[test]
fn merging_bounds_segment_count_and_keeps_every_span() {
    let dir = test_dir("bound");
    let store = Store::open(&dir, eager(4)).expect("opens");

    for batch in 0..8 {
        seal(
            &store,
            (0..10)
                .map(|i| {
                    let n = batch * 10 + i;
                    span(&format!("t{n}"), "s1", "op", 1_000 + n as u64)
                })
                .collect(),
        );
    }
    assert_eq!(
        store.stats().expect("stats").segment_count,
        8,
        "one segment per flush before compaction"
    );

    let removed = store.compact_segments().expect("compacts");
    let after = store.stats().expect("stats").segment_count;
    assert!(after < 8, "compaction reduced segments: {after}");
    assert_eq!(removed, 8 - after, "removed count matches the reduction");

    // The whole point: same data, fewer files.
    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 80, "no span lost or duplicated by the merge");
    for n in 0..80 {
        assert_eq!(
            store.get_trace(&format!("t{n}")).expect("trace").len(),
            1,
            "trace t{n} survives the merge"
        );
    }
}

#[test]
fn a_merge_keeps_the_newest_version_of_a_re_ingested_span() {
    // The correctness crux. Segment order is recency order, so a merge must
    // resolve the primary key the same way a read would.
    let dir = test_dir("lww");
    let store = Store::open(&dir, eager(4)).expect("opens");

    for (round, name) in ["first", "second", "third", "fourth"].iter().enumerate() {
        seal(&store, vec![span("t", "s", name, 1_000 + round as u64)]);
    }
    assert_eq!(store.stats().expect("stats").segment_count, 4);

    store.compact_segments().expect("compacts");
    assert_eq!(
        store.stats().expect("stats").segment_count,
        1,
        "four same-tier segments merge into one"
    );

    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1, "one primary key, one span: {trace:?}");
    assert_eq!(
        trace[0].name, "fourth",
        "the newest version survives the merge"
    );
}

#[test]
fn a_newer_unmerged_segment_still_wins_over_merged_content() {
    // Compaction only ever merges the TAIL, because a merged segment takes a
    // fresh (newest) id. This proves the rule it protects: content that was
    // superseded must stay superseded.
    let dir = test_dir("tail-order");
    let store = Store::open(&dir, eager(4)).expect("opens");

    for (round, name) in ["v1", "v2", "v3", "v4"].iter().enumerate() {
        seal(&store, vec![span("t", "s", name, 1_000 + round as u64)]);
    }
    store.compact_segments().expect("compacts");
    // A newer write lands after the merged segment.
    seal(&store, vec![span("t", "s", "v5", 2_000)]);

    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].name, "v5", "the post-merge write wins: {trace:?}");

    // And merging again keeps it.
    store.compact_segments().expect("compacts");
    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].name, "v5", "still wins after a second merge");
}

#[test]
fn compaction_survives_reopen_and_leaves_no_journal_behind() {
    let dir = test_dir("reopen");
    {
        let store = Store::open(&dir, eager(3)).expect("opens");
        for batch in 0..6 {
            seal(
                &store,
                (0..5)
                    .map(|i| {
                        let n = batch * 5 + i;
                        span(&format!("t{n}"), "s1", "op", 1_000 + n as u64)
                    })
                    .collect(),
            );
        }
        store.compact_segments().expect("compacts");
    }

    // No half-finished merge state may remain on disk.
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".supersede.") || name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "merge left artifacts: {leftovers:?}");

    let store = Store::open(&dir, eager(3)).expect("reopens");
    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 30, "every span survives compaction + reopen");
}

#[test]
fn disabled_compaction_leaves_every_segment_alone() {
    let dir = test_dir("disabled");
    let store = Store::open(
        &dir,
        Config {
            durability: Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");

    for batch in 0..5 {
        seal(&store, vec![span(&format!("t{batch}"), "s1", "op", 1_000)]);
    }
    assert_eq!(store.compact_segments().expect("no-op"), 0);
    assert_eq!(
        store.stats().expect("stats").segment_count,
        5,
        "compaction off means segments stay as flushed"
    );
}

#[test]
fn a_fanout_below_two_is_treated_as_disabled() {
    // fanout 1 would "merge" a single segment forever; refuse rather than spin.
    let dir = test_dir("fanout-one");
    let store = Store::open(&dir, eager(1)).expect("opens");
    for batch in 0..4 {
        seal(&store, vec![span(&format!("t{batch}"), "s1", "op", 1_000)]);
    }
    assert_eq!(store.compact_segments().expect("no-op"), 0);
    assert_eq!(store.stats().expect("stats").segment_count, 4);
}

#[test]
fn the_size_cap_stops_runaway_merges() {
    // With a cap far below the run's total, the merge must not produce one
    // giant segment.
    let dir = test_dir("cap");
    let store = Store::open(
        &dir,
        Config {
            durability: Durability::Buffered,
            compaction: Some(CompactionConfig {
                fanout: 2,
                base_bytes: 1,
                max_segment_bytes: 1, // every segment already exceeds this
            }),
            ..Config::default()
        },
    )
    .expect("opens");

    for batch in 0..6 {
        seal(&store, vec![span(&format!("t{batch}"), "s1", "op", 1_000)]);
    }
    let before = store.stats().expect("stats").segment_count;
    store.compact_segments().expect("compacts");
    let after = store.stats().expect("stats").segment_count;
    assert_eq!(
        after, before,
        "a cap smaller than any segment blocks merging instead of looping"
    );
    // Data is still intact and queryable.
    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 6);
}

#[test]
fn compaction_and_ttl_expiry_coexist() {
    // Both rewrite segments through the same journal; running them together
    // must not strand data or artifacts.
    let dir = test_dir("ttl");
    let store = Store::open(
        &dir,
        Config {
            durability: Durability::Buffered,
            ttl_seconds: Some(3_600),
            compaction: Some(CompactionConfig {
                fanout: 2,
                base_bytes: 1,
                max_segment_bytes: 0,
            }),
            ..Config::default()
        },
    )
    .expect("opens");

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    // Half old enough to expire, half recent.
    for batch in 0..4 {
        seal(
            &store,
            vec![span(
                &format!("old{batch}"),
                "s1",
                "op",
                now_ns - 7_200 * 1_000_000_000,
            )],
        );
    }
    for batch in 0..4 {
        seal(
            &store,
            vec![span(&format!("new{batch}"), "s1", "op", now_ns)],
        );
    }

    store.compact_segments().expect("compacts");
    store.compact_expired().expect("expires");
    store.compact_segments().expect("compacts again");

    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert!(
        all.iter().all(|span| span.trace_id.starts_with("new")),
        "expired spans are gone: {:?}",
        all.iter().map(|s| &s.trace_id).collect::<Vec<_>>()
    );
    assert_eq!(all.len(), 4, "recent spans all survive both passes");
}

/// A segment big enough that merging it is measurable work rather than an
/// instant. Compaction is only interesting to the rest of the engine when it
/// takes long enough to be in the way.
fn bulky_span(trace: &str, id: &str, start_ns: u64) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace, "span_id": id, "name": "op", "service": "svc",
        "start_time_ns": start_ns, "end_time_ns": start_ns + 100,
        "attributes": {"group": "g1", "filler": "x".repeat(256)}
    }))
    .expect("span")
}

#[test]
fn reads_and_ingest_continue_while_a_merge_runs() {
    // Found in review: the merge held the segment lock across parsing every
    // input, materializing the union, and fsyncing the replacement. Queries
    // waited for that lock while holding the writer lock, so ingest queued
    // behind them — a multi-gigabyte merge was a multi-gigabyte outage.
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    let dir = test_dir("concurrent-merge");
    let store = Arc::new(Store::open(&dir, eager(4)).expect("opens"));
    for segment in 0..4u64 {
        seal(
            &store,
            (0..4_000)
                .map(|index| {
                    let id = format!("s{segment}-{index}");
                    bulky_span("t-bulk", &id, 1_000 + segment * 10_000 + index)
                })
                .collect(),
        );
    }
    // The probe stays in the write buffer: sealing it would leave a tiny
    // segment at the tail and no same-tier run to merge.
    store
        .ingest(span("t-probe", "probe", "probe", 500))
        .expect("ingest");

    let merging = Arc::new(AtomicBool::new(true));
    let compactor = {
        let store = Arc::clone(&store);
        let merging = Arc::clone(&merging);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let merged = store.compact_segments().expect("compacts");
            merging.store(false, Ordering::SeqCst);
            (merged, started.elapsed())
        })
    };

    // The number that decides this is the SLOWEST operation, not the total.
    // A reader that queues behind the merge still completes a burst of fast
    // operations the moment the lock is released, so throughput alone cannot
    // tell "never blocked" from "blocked once for the whole merge".
    let completed = AtomicUsize::new(0);
    let mut slowest = std::time::Duration::ZERO;
    let mut ingested = 0u64;
    while merging.load(Ordering::SeqCst) {
        let started = std::time::Instant::now();
        let probe = store.get_trace("t-probe").expect("read during merge");
        store
            .ingest(span("t-probe", &format!("live-{ingested}"), "live", 600))
            .expect("ingest during merge");
        let elapsed = started.elapsed();
        ingested += 1;
        assert!(!probe.is_empty(), "the probe span stays visible");
        if merging.load(Ordering::SeqCst) {
            completed.fetch_add(1, Ordering::Relaxed);
            slowest = slowest.max(elapsed);
        }
    }
    let (merged_away, merge_elapsed) = compactor.join().expect("compactor");

    assert!(merged_away > 0, "the merge actually happened");
    assert!(
        merge_elapsed >= std::time::Duration::from_millis(100),
        "the merge must be slow enough to be in the way, took {merge_elapsed:?}"
    );
    assert!(
        slowest * 3 < merge_elapsed,
        "no read or ingest may wait out the merge: slowest {slowest:?} against a \
         {merge_elapsed:?} merge, over {} operations",
        completed.load(Ordering::SeqCst)
    );
    // And nothing the merge or the concurrent writers did was lost.
    let probe = store.get_trace("t-probe").expect("trace");
    assert_eq!(probe.len(), ingested as usize + 1);
    assert_eq!(
        store
            .query(&SpanFilter {
                name: Some("op".into()),
                ..SpanFilter::default()
            })
            .expect("query")
            .len(),
        16_000,
        "every merged span survives"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn a_flush_during_a_merge_still_supersedes_merged_content() {
    // The merge no longer holds the segment lock, so a flush can land while it
    // runs. Segment order is recency order, so the merged output must sort
    // BEFORE that flush — otherwise re-ingested spans revert to the version
    // the merge happened to capture.
    use std::sync::Arc;

    for round in 0..4u64 {
        let dir = test_dir(&format!("merge-vs-flush-{round}"));
        let store = Arc::new(Store::open(&dir, eager(4)).expect("opens"));
        for segment in 0..4u64 {
            let mut spans: Vec<Span> = (0..2_000)
                .map(|index| {
                    let id = format!("s{segment}-{index}");
                    bulky_span("t-bulk", &id, 1_000 + segment * 10_000 + index)
                })
                .collect();
            // Every segment carries the same 20 hot keys, so the merge has a
            // version of each to lose.
            spans.extend((0..20).map(|key| span("t-hot", &format!("k{key}"), "old", 900)));
            seal(&store, spans);
        }

        let compactor = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.compact_segments().expect("compacts"))
        };
        // Replace every hot key while the merge is in flight.
        for key in 0..20 {
            store
                .ingest(span("t-hot", &format!("k{key}"), "new", 900))
                .expect("ingest");
        }
        store.flush().expect("flush");
        compactor.join().expect("compactor");

        let hot = store.get_trace("t-hot").expect("trace");
        assert_eq!(hot.len(), 20, "one version per key: {}", hot.len());
        assert!(
            hot.iter().all(|span| span.name == "new"),
            "the flush is newer than the merge and must win: {:?}",
            hot.iter().map(|span| &span.name).collect::<Vec<_>>()
        );
        drop(store);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
