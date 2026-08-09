//! Size-tiered compaction: it must bound segment count WITHOUT changing a
//! single answer.
//!
//! Merging rewrites the physical layout that last-write-wins depends on —
//! segment order is recency order — so every test here checks the data as
//! well as the segment count. A compaction that loses the newest version of a
//! re-ingested span would be far worse than the slow search it fixes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// Copies a one-span segment into `dir` under `name`, standing in for an
/// output a merge had already written when it was interrupted.
fn stage_output(dir: &Path, name: &str, version: &str) {
    let staging = test_dir("staging");
    let helper = Store::open(&staging, tiered(0)).expect("opens");
    seal(&helper, vec![span("t", "s", version, 1_000)]);
    drop(helper);
    let staged = std::fs::read_dir(&staging)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("staged segment");
    std::fs::copy(&staged, dir.join(name)).expect("copy");
    std::fs::remove_dir_all(&staging).expect("cleanup");
}

fn segment_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".seg"))
        .collect();
    names.sort();
    names
}

fn write_journal(dir: &Path, inputs: &[String], outputs: &[&str]) {
    std::fs::write(
        dir.join(format!(".supersede.{}.journal", outputs[0])),
        format!(
            "inputs {}\noutputs {}\n",
            inputs.join(","),
            outputs.join(",")
        ),
    )
    .expect("journal");
}

fn journals(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".supersede."))
        .collect()
}

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
        // Whichever way the race went, the directory holds exactly the live
        // segments and no journal. A merge that abandons publication has to
        // take its outputs back off disk before dropping the journal that
        // describes them — an orphan output outranks every input by id, so
        // one left behind would be loaded at the next open and shadow them.
        let on_disk = segment_names(&dir).len();
        assert_eq!(
            on_disk,
            store.stats().expect("stats").segment_count,
            "an abandoned merge left an output behind"
        );
        assert!(
            journals(&dir).is_empty(),
            "journals must be cleared: {:?}",
            journals(&dir)
        );
        drop(store);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}

/// Compaction that keeps the tiers meaningful: a 100-span segment (~31 KB)
/// lands a tier above `base_bytes`, and a handful of spans lands below it —
/// the same split the 8 MiB default produces between a full flush and a
/// partial one.
fn tiered(max_segment_bytes: u64) -> Config {
    Config {
        durability: Durability::Buffered,
        compaction: Some(CompactionConfig {
            fanout: 4,
            base_bytes: 20_000,
            max_segment_bytes,
        }),
        ..Config::default()
    }
}

fn batch(store: &Store, count: usize, base: u64) {
    seal(
        store,
        (0..count as u64)
            .map(|i| {
                let n = base + i;
                span(&format!("t{n}"), "s1", "op", 1_000 + n)
            })
            .collect(),
    );
}

#[test]
fn a_partial_final_segment_does_not_stall_compaction() {
    // Ingest seals when the write buffer hits `flush_spans`, and batches do
    // not divide evenly, so the last segment of a finished load is a partial
    // one. Below `base_bytes` it is a tier of its own, and anchoring the run
    // on the LAST segment's tier made that a wall: the run was length 1,
    // under `fanout`, and a store that stopped ingesting mid-segment — a bulk
    // load, a seeded corpus, an archived store — kept every segment forever.
    let dir = test_dir("partial-tail");
    let store = Store::open(&dir, tiered(0)).expect("opens");

    for full in 0..8u64 {
        batch(&store, 100, full * 1_000);
    }
    // The partial flush: far below `base_bytes`, so a tier below its peers.
    batch(&store, 3, 900_000);
    assert_eq!(store.stats().expect("stats").segment_count, 9);

    let removed = store.compact_segments().expect("compacts");
    assert_eq!(removed, 8, "the whole run merges, partial tail included");
    assert_eq!(store.stats().expect("stats").segment_count, 1);

    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 803, "every span survives, partial ones included");
}

#[test]
fn a_partial_segment_in_the_middle_does_not_wall_off_the_prefix() {
    // Same segment, no longer at the tail: ingest resumed after the store
    // stopped. Nothing behind the tail is ever a merge candidate again, so a
    // wall here froze the whole prefix permanently rather than briefly.
    let dir = test_dir("partial-middle");
    let store = Store::open(&dir, tiered(0)).expect("opens");

    for full in 0..8u64 {
        batch(&store, 100, full * 1_000);
    }
    batch(&store, 3, 900_000);
    // Ingest resumes, burying the partial segment mid-list.
    for full in 0..4u64 {
        batch(&store, 100, 100_000 + full * 1_000);
    }

    store.compact_segments().expect("compacts");
    assert_eq!(
        store.stats().expect("stats").segment_count,
        1,
        "the buried partial segment is absorbed, not merged around"
    );

    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 1_203);
}

#[test]
fn a_backlog_compacts_down_to_the_size_cap() {
    // The cap bounds each OUTPUT, not the run. Capping by truncating the run
    // left a cap-sized segment at the tail with smaller ones behind it, and
    // those were unreachable forever — one merge, then permanently stuck.
    let dir = test_dir("backlog");
    // ~500 KB, about 16 segments' worth.
    let store = Store::open(&dir, tiered(500_000)).expect("opens");

    for full in 0..60u64 {
        batch(&store, 100, full * 1_000);
    }
    assert_eq!(store.stats().expect("stats").segment_count, 60);
    let disk = store.stats().expect("stats").disk_bytes;

    let removed = store.compact_segments().expect("compacts");
    let after = store.stats().expect("stats").segment_count;
    assert_eq!(removed, 60 - after);
    // Four ~500 KB outputs for ~1.9 MB of input: the floor the cap implies,
    // not the 46 that stopping the run at the cap used to leave.
    // (d + n - 1) / n rather than div_ceil: the crate's MSRV is 1.70.
    let floor = disk.div_ceil(500_000) as usize;
    assert!(
        after <= floor + 1,
        "backlog left {after} segments for {disk} bytes at a 500 KB cap \
         (floor {floor})"
    );

    // And it is a fixed point: no further pass finds anything worth doing.
    assert_eq!(store.compact_segments().expect("compacts"), 0);
    assert_eq!(store.stats().expect("stats").segment_count, after);

    let all = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(all.len(), 6_000, "every span survives a grouped merge");
}

#[test]
fn a_grouped_merge_resolves_the_primary_key_across_its_outputs() {
    // A run that splits into several outputs dedups WITHIN each group, not
    // across them, so a key written in two groups lands in two outputs. What
    // keeps last-write-wins intact is that the outputs take ids in group
    // order — the same order their inputs had.
    let dir = test_dir("grouped-lww");
    // Small enough that 12 segments cannot become one.
    let store = Store::open(&dir, tiered(120_000)).expect("opens");

    // One key rewritten in every segment, plus bulk to force several groups.
    for round in 0..12u64 {
        let mut spans = vec![span("t", "s", &format!("v{round}"), 1_000)];
        spans.extend((0..100u64).map(|i| {
            let n = round * 1_000 + i;
            span(&format!("bulk{n}"), "s1", "op", 2_000 + n)
        }));
        seal(&store, spans);
    }
    assert_eq!(store.stats().expect("stats").segment_count, 12);

    store.compact_segments().expect("compacts");
    let after = store.stats().expect("stats").segment_count;
    assert!(
        (2..12).contains(&after),
        "the cap must split this run into several outputs, got {after}"
    );

    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1, "one primary key, one span: {trace:?}");
    assert_eq!(
        trace[0].name, "v11",
        "the newest version wins across group boundaries"
    );

    // And a write after the grouped merge still supersedes all of it.
    seal(&store, vec![span("t", "s", "v12", 1_000)]);
    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace[0].name, "v12");
}

#[test]
fn a_half_written_output_group_is_rolled_back_on_reopen() {
    // A grouped merge is atomic or it is nothing. An output holds only its
    // own group's view of a key, and it carries a higher id than every input,
    // so one left beside intact inputs would shadow a newer version living in
    // a group whose output never landed. Recovery must delete it, not keep it.
    let dir = test_dir("rollback");
    let store = Store::open(&dir, tiered(0)).expect("opens");
    for round in 0..4u64 {
        seal(&store, vec![span("t", "s", &format!("v{round}"), 1_000)]);
    }
    let inputs = segment_names(&dir);
    assert_eq!(inputs.len(), 4);
    drop(store);

    // The wreckage of a crash midway through publishing a two-output group:
    // the first output landed, the second never did, and EVERY input is still
    // on disk because a merge deletes those last. The stand-in output holds
    // the stale version, which is what a first group would hold.
    let landed = "segment-00000000000000000099.seg";
    let missing = "segment-00000000000000000100.seg";
    stage_output(&dir, landed, "v0");
    write_journal(&dir, &inputs, &[landed, missing]);

    let store = Store::open(&dir, tiered(0)).expect("reopens");
    assert!(
        !dir.join(landed).exists(),
        "the output that landed must be rolled back, not kept"
    );
    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1);
    assert_eq!(
        trace[0].name, "v3",
        "the inputs stay authoritative, so the newest version still wins"
    );
    assert!(
        journals(&dir).is_empty(),
        "journals must be cleared: {:?}",
        journals(&dir)
    );
}

#[test]
fn a_merge_whose_input_deletion_had_started_rolls_forward() {
    // The state a failed unlink leaves. Publication succeeded, so every
    // output was durable; deleting the inputs then failed partway, the error
    // was logged and the server kept running, and a later merge consumed one
    // of the outputs. On reopen one input is gone and one output is missing.
    //
    // Deciding that per input would read the surviving input as "nothing was
    // deleted yet" and roll back — deleting live outputs that hold the only
    // copy of the input already removed. An input already gone is proof of
    // the opposite, and it is a fact about the group, not about any one file.
    let dir = test_dir("roll-forward");
    let store = Store::open(&dir, tiered(0)).expect("opens");
    seal(&store, vec![span("trace-a", "s", "kept-a", 1_000)]);
    seal(&store, vec![span("trace-b", "s", "kept-b", 2_000)]);
    let inputs = segment_names(&dir);
    assert_eq!(inputs.len(), 2);
    drop(store);

    // Input A was unlinked, input B was not. Output 1 survives; output 2 has
    // since been consumed by a later merge, whose own output carries its data
    // — represented here by A's spans living on in the surviving output.
    let survived = "segment-00000000000000000099.seg";
    let consumed = "segment-00000000000000000100.seg";
    stage_output(&dir, survived, "merged");
    std::fs::remove_file(dir.join(&inputs[0])).expect("unlink A");
    write_journal(&dir, &inputs, &[survived, consumed]);

    let store = Store::open(&dir, tiered(0)).expect("reopens");
    assert!(
        dir.join(survived).exists(),
        "a live output must not be rolled back once deletion had started"
    );
    assert!(
        !dir.join(&inputs[1]).exists(),
        "the deletion resumes instead: input B is finished off"
    );
    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1, "the surviving output's data is intact");
    assert_eq!(trace[0].name, "merged");
    assert!(
        journals(&dir).is_empty(),
        "journals must be cleared: {:?}",
        journals(&dir)
    );
}

#[test]
fn a_stale_journal_does_not_roll_back_a_merge_that_committed() {
    // A journal that outlives the merge it describes, after a later merge
    // consumed one of its outputs. Every input it names is gone, which is
    // what tells recovery the merge committed and its outputs are live.
    let dir = test_dir("stale-journal");
    let store = Store::open(&dir, tiered(0)).expect("opens");
    for round in 0..4u64 {
        seal(&store, vec![span("t", "s", &format!("v{round}"), 1_000)]);
    }
    store.compact_segments().expect("compacts");
    let live = segment_names(&dir);
    assert_eq!(live.len(), 1, "the merge committed: {live:?}");
    drop(store);

    let inputs: Vec<String> = (0..4).map(|id| format!("segment-{id:020}.seg")).collect();
    assert!(inputs.iter().all(|name| !dir.join(name).exists()));
    write_journal(
        &dir,
        &inputs,
        &[&live[0], "segment-00000000000000009999.seg"],
    );

    let store = Store::open(&dir, tiered(0)).expect("reopens");
    assert!(
        dir.join(&live[0]).exists(),
        "a live output must survive a stale journal"
    );
    let trace = store.get_trace("t").expect("trace");
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].name, "v3", "and the data with it");
}

#[test]
fn compaction_keeps_up_with_a_store_that_never_stops_ingesting() {
    // Compaction used to DECLINE while a seal held an unpublished id, rather
    // than wait for it. That is a correctness requirement — a seal holding a
    // lower id publishes newer data, and merged output claiming a higher one
    // would outrank it — but declining made it hostage to the write rate. A
    // seal is in flight for much of the time under load, so a tick that
    // checked once and gave up almost never found the store quiet: one tick
    // in sixteen achieved anything at 25,000 spans/s, and the segment count
    // climbed without bound while compaction ran on schedule and did nothing.
    //
    // A merge now waits for the seal permit, for the microseconds it takes to
    // choose a run and claim its ids.
    let dir = test_dir("sustained");
    let store = Arc::new(
        Store::open(
            &dir,
            Config {
                durability: Durability::Buffered,
                flush_spans: 200,
                compaction: Some(CompactionConfig {
                    fanout: 4,
                    base_bytes: 8 * 1024 * 1024,
                    max_segment_bytes: 256 * 1024 * 1024,
                }),
                ..Config::default()
            },
        )
        .expect("opens"),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3u64)
        .map(|worker| {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    store
                        .ingest(span(
                            &format!("t{worker}"),
                            &format!("s-{worker}-{n}"),
                            "op",
                            1_000 + n,
                        ))
                        .expect("ingest");
                }
                n
            })
        })
        .collect();

    let mut productive = 0;
    let mut peak = 0;
    const TICKS: usize = 6;
    for _ in 0..TICKS {
        std::thread::sleep(Duration::from_millis(250));
        // A tick compacts the backlog it found and RETURNS. Merging what
        // arrives while it runs would keep it going for as long as the writes
        // do — measured at 2,213 segments merged away in a single call that
        // never came back. That failure cannot be caught after the call, so
        // the call is given a deadline of its own.
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let _ = sender.send(store.compact_segments());
            });
        }
        let removed = match receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(result) => result.expect("compacts"),
            Err(_) => {
                // Let the writers stop so the runaway merge can drain.
                stop.store(true, Ordering::Relaxed);
                panic!(
                    "a tick did not return within 20s while writes continued: \
                     it is merging what arrives instead of the backlog it found"
                );
            }
        };
        if removed > 0 {
            productive += 1;
        }
        peak = peak.max(store.stats().expect("stats").segment_count);
    }
    stop.store(true, Ordering::Relaxed);
    let ingested: u64 = writers
        .into_iter()
        .map(|handle| handle.join().expect("writer"))
        .sum();

    // Every tick found work and did it. The old behaviour managed one in
    // sixteen, so even a much weaker bound than "all of them" separates them.
    assert!(
        productive >= TICKS - 1,
        "only {productive} of {TICKS} ticks compacted anything under load"
    );
    // And the segment count stayed near what the flush threshold implies
    // rather than tracking the whole run: unbounded growth is the symptom.
    let seals = ingested / 200;
    assert!(
        (peak as u64) < seals / 2,
        "segments peaked at {peak} against {seals} seals — compaction is not \
         keeping up"
    );

    // Nothing was lost to all that merging under load. Counted through
    // `stats` rather than a query: every span_id here is unique, so no
    // physical record is a superseded version and the physical count IS the
    // logical one — and a query for hundreds of thousands of rows across this
    // many segments costs far more than the fact is worth.
    store.flush().expect("flush");
    store.compact_segments().expect("compacts");
    let stats = store.stats().expect("stats");
    assert_eq!(stats.buffered_records, 0, "the flush emptied the buffer");
    assert_eq!(
        stats.total_records as u64, ingested,
        "every ingested span survives"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
