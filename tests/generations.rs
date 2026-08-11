//! The generation boundary, exercised through the public engine API.
//!
//! These prove the M2 contract from the outside: an old-layout directory is
//! adopted at first open with its data intact; a checkpoint publishes a
//! generation that a reopen resolves through; a published deletion stays
//! deleted across a reopen because the log's fold rule excludes the frames it
//! removed; and backup-by-pin then restore round-trips a live store's whole
//! state — spans, annotations, and payload bytes together. Process-level
//! SIGKILL durability across a checkpoint lives in `tests/durability.rs`,
//! beside the rest of the kill suite and its harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use traza::{Config, Durability, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-gen-it-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span(trace: &str, id: &str, marker: &str) -> traza::Span {
    serde_json::from_value(serde_json::json!({
        "trace_id": trace, "span_id": id, "name": "s", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": {"marker": marker},
    }))
    .expect("span")
}

fn wal_config() -> Config {
    Config {
        durability: Durability::Wal,
        ..Config::default()
    }
}

fn marker_count(store: &Store, marker: &str) -> usize {
    store
        .query(&SpanFilter {
            attributes: vec![("marker".into(), marker.into())],
            ..SpanFilter::default()
        })
        .expect("query")
        .len()
}

#[test]
fn a_fresh_store_publishes_generation_one() {
    let dir = test_dir("fresh");
    let store = Store::open(&dir, wal_config()).expect("open");
    assert_eq!(
        store.live_generation(),
        1,
        "a new store starts at generation one"
    );
    assert!(dir.join("CURRENT").exists());
    assert!(
        dir.join("generations/1/state-manifest.json").exists(),
        "generation one's manifest is on disk"
    );
    drop(store);
}

#[test]
fn a_pre_generation_directory_is_adopted_with_its_data() {
    // Build a store, then simulate a pre-generation layout by deleting the
    // generation metadata the way an old binary's directory would not have
    // it. The engine files stay exactly where they are — which is the whole
    // point of adopting in place — so removing CURRENT and generations/ is a
    // faithful stand-in for a directory written before this code existed.
    let dir = test_dir("adopt");
    {
        let store = Store::open(&dir, wal_config()).expect("open");
        store
            .ingest_batch(vec![span("t", "a", "keep"), span("t", "b", "keep")])
            .expect("ingest");
        store.flush().expect("flush");
        drop(store);
    }
    fs::remove_file(dir.join("CURRENT")).expect("rm CURRENT");
    fs::remove_dir_all(dir.join("generations")).expect("rm generations");
    // Also strip the log magic back out, so adoption exercises the v1 log
    // conversion rather than finding a v2 log already in place.
    // (A flushed store has an empty log, so there is nothing to convert here;
    // the WAL unit tests cover the populated conversion.)

    let store = Store::open(&dir, wal_config()).expect("reopen adopts");
    assert_eq!(store.live_generation(), 1);
    assert_eq!(marker_count(&store, "keep"), 2, "adopted data is intact");
    // Idempotent: a second open finds CURRENT and does not re-adopt.
    drop(store);
    let store = Store::open(&dir, wal_config()).expect("second open");
    assert_eq!(marker_count(&store, "keep"), 2);
}

#[test]
fn a_checkpoint_publishes_a_generation_a_reopen_resolves_through() {
    let dir = test_dir("checkpoint");
    let store = Store::open(&dir, wal_config()).expect("open");
    store
        .ingest_batch(vec![span("t", "a", "one"), span("t", "b", "one")])
        .expect("ingest");
    let gen = store.checkpoint().expect("checkpoint");
    assert!(gen > 1, "the checkpoint advances the generation: {gen}");
    assert_eq!(store.live_generation(), gen);
    assert!(
        store.verify_generation(gen).expect("verify").is_empty(),
        "the just-published generation verifies clean"
    );
    drop(store);

    let store = Store::open(&dir, wal_config()).expect("reopen");
    assert_eq!(
        store.live_generation(),
        gen,
        "reopen loads the checkpointed generation"
    );
    assert_eq!(marker_count(&store, "one"), 2);
}

#[test]
fn a_published_deletion_stays_deleted_across_reopen() {
    // The property the whole boundary exists for: a deletion becomes durable
    // when CURRENT moves, and the log's fold rule keeps the removed frames
    // from replaying it back into existence even though nothing rewrote the
    // log to physically drop them at expiry time.
    let dir = test_dir("deletion");
    let store = Store::open(&dir, wal_config()).expect("open");
    // A span old enough to expire, and one that is not.
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let old: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "old", "name": "s", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": {"marker": "old"},
    }))
    .expect("span");
    let fresh: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "new", "name": "s", "service": "svc",
        "start_time_ns": now_ns, "end_time_ns": now_ns + 1_000,
        "attributes": {"marker": "fresh"},
    }))
    .expect("span");
    store.ingest_batch(vec![old, fresh]).expect("ingest");

    // Expire everything older than a minute ago: the old span goes, the fresh
    // one stays. Expiry removes from every domain it touches; the checkpoint
    // is what PUBLISHES that removal as a generation, and it seals the
    // survivors into a segment on the way — so after it, the log's frames for
    // both spans are folded and must never replay. If the fold rule were
    // wrong, the expired span would come back on the reopen below.
    let cutoff = now_ns - 60_000_000_000;
    let removed = store.expire_before(cutoff).expect("expire");
    assert_eq!(removed, 1, "exactly the old span expired");
    let gen = store
        .checkpoint()
        .expect("checkpoint publishes the deletion");
    assert_eq!(marker_count(&store, "old"), 0);
    assert_eq!(marker_count(&store, "fresh"), 1);
    drop(store);

    // Reopen: the deletion must hold. If the log replayed its folded frames,
    // the old span would return.
    let store = Store::open(&dir, wal_config()).expect("reopen");
    assert_eq!(store.live_generation(), gen);
    assert_eq!(
        marker_count(&store, "old"),
        0,
        "the deletion survived the reopen"
    );
    assert_eq!(marker_count(&store, "fresh"), 1);
}

#[test]
fn folded_frames_left_in_the_log_by_a_crash_never_replay() {
    // The rule the stamp exists for, and the only test that actually
    // exercises it. `CURRENT` and the log are separate filesystem objects, so
    // a crash can land after the checkpoint's `CURRENT` fsync but before the
    // roll-over that drops the folded frames. Recovery then meets a log still
    // physically holding frames the live generation already contains — and
    // for a checkpoint that published a DELETION, replaying them would
    // resurrect exactly what was deleted.
    //
    // That state is reconstructed here rather than raced for: the log's bytes
    // are captured before the checkpoint and written back after it, which is
    // byte-for-byte what the crash leaves behind.
    let dir = test_dir("folded-replay");
    let store = Store::open(&dir, wal_config()).expect("open");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let old: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "old", "name": "s", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": {"marker": "old"},
    }))
    .expect("span");
    let fresh: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "new", "name": "s", "service": "svc",
        "start_time_ns": now_ns, "end_time_ns": now_ns + 1_000,
        "attributes": {"marker": "fresh"},
    }))
    .expect("span");
    store.ingest_batch(vec![old, fresh]).expect("ingest");

    // The log as it stands with BOTH spans in it — what a crash would leave.
    let log_path = dir.join("wal.log");
    let before_checkpoint = fs::read(&log_path).expect("read log");
    assert!(
        before_checkpoint.len() > 8,
        "the log holds frames to fold: {} bytes",
        before_checkpoint.len()
    );

    store
        .expire_before(now_ns - 60_000_000_000)
        .expect("expire");
    let gen = store.checkpoint().expect("checkpoint");
    drop(store);

    // Put the folded frames back: CURRENT is durable at the new generation,
    // and the log still holds every frame that went into it.
    fs::write(&log_path, &before_checkpoint).expect("restore pre-checkpoint log");

    let store = Store::open(&dir, wal_config()).expect("reopen");
    assert_eq!(store.live_generation(), gen);
    assert_eq!(
        marker_count(&store, "old"),
        0,
        "a folded frame must not replay a published deletion back into existence"
    );
    assert_eq!(
        marker_count(&store, "fresh"),
        1,
        "and the surviving span is present exactly once, not duplicated by its folded frame"
    );
}

#[test]
fn backup_by_pin_then_restore_round_trips_the_whole_store() {
    // Backup is pin, verify, copy — no server stop. Restore is install. This
    // proves the pinned set carries spans, annotations, AND payload bytes
    // together, which is the export/backup gap the boundary closes.
    let source = test_dir("backup-src");
    let backup = test_dir("backup-copy");
    let restored = test_dir("backup-dst");
    fs::remove_dir_all(&backup).ok();
    fs::remove_dir_all(&restored).ok();

    // Offloading is what puts bytes in `payloads/`, and that is the domain a
    // span export could never pin. The threshold is set low here so the test
    // states its own precondition rather than depending on the default.
    let offloading = Config {
        payload_threshold: Some(1024),
        ..wal_config()
    };
    let big_prompt = "x".repeat(4096);
    let with_payload: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "p", "name": "s", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": {"marker": "payload", "gen_ai.prompt": big_prompt},
    }))
    .expect("span");

    let store = Store::open(&source, offloading.clone()).expect("open");
    store
        .ingest_batch(vec![span("t", "a", "kept"), with_payload])
        .expect("ingest");
    store.flush().expect("flush");
    store
        .annotate(traza::annotations::Annotation {
            trace_id: "t".into(),
            span_id: "a".into(),
            name: "score".into(),
            value: serde_json::json!(1),
            source: "eval:test".into(),
            comment: String::new(),
            timestamp_ns: 100,
        })
        .expect("annotate");

    // Backup, live.
    store.pin_generation("backup").expect("pin");
    assert!(
        store.verify_pin("backup").expect("verify").is_empty(),
        "the pin verifies before we trust the copy"
    );
    copy_tree(&source.join("pins/backup"), &backup);
    store.release_pin("backup").expect("release");
    // The store keeps working after the pin is released.
    assert_eq!(marker_count(&store, "kept"), 1);
    drop(store);

    // Restore into a fresh directory and confirm every domain came back.
    let store = Store::restore(&restored, &backup, offloading).expect("restore");
    assert_eq!(marker_count(&store, "kept"), 1, "spans restored");
    let payload_spans = store
        .query(&SpanFilter {
            attributes: vec![("marker".into(), "payload".into())],
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(
        payload_spans.len(),
        1,
        "the offloaded-payload span restored"
    );
    // The payload bytes themselves came across, not just the reference.
    let restored_payload = &payload_spans[0];
    let reference = restored_payload
        .attributes
        .get("gen_ai.prompt")
        .and_then(|value| value.get("$payload"))
        .and_then(|value| value.as_str())
        .expect("payload reference survived");
    let bytes = store.payload(reference).expect("load payload");
    assert_eq!(
        bytes.map(|b| b.len()),
        Some(4096),
        "the offloaded payload bytes restored, not just the reference"
    );
    let annotations = store.annotations("t", None, None).expect("annotations");
    assert_eq!(annotations.len(), 1, "annotations restored");
}

/// Recursively copies a directory tree — the "copy the pin" half of a backup.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}
