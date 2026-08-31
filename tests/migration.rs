//! Acceptance gate 4 of format v7: the v6 → v7 migration.
//!
//! The contract under test is the Migration section of
//! `docs/segment-format.md`: a v6 store (segments, blobs, sidecars, pins,
//! non-empty WAL) opened by this build serves query-identical results,
//! preserves segment names, survives `kill -9` at the crash-matrix points
//! with a clean resume, and publishes a completion checkpoint whose
//! generation verifies clean — `/v1/verify` reports `intact: true` and a pin
//! taken immediately after migration passes its verify-at-pin, which is the
//! half of the gate that catches a completion checkpoint carrying digests of
//! bytes the migration replaced. Erasure records ride across the migration:
//! a pending one settles against the migrated store, and a settled receipt
//! re-derives.
//!
//! # Where the v6 fixtures come from
//!
//! The committed pr50 fixture (driven by `tests/storage.rs`) is real v6
//! bytes from a real old build, and it stays the ground truth for the raw
//! format. It is also tiny: two segments, no blobs, no pins, no WAL — not
//! the store gate 4 describes. The richer fixtures here are built by this
//! build and then DOWNGRADED: every v7 segment is re-encoded through a
//! byte-faithful copy of the v6 encoder (from `src/segment.rs` at commit
//! `5f23172`, the frozen decoder's inverse), every `TRZBLOB1` blob is
//! replaced by its raw content bytes (the v6 blob format IS the content, and
//! the builder recorded every offloaded value), and the manifests are
//! re-digested over the downgraded bytes with the format declaration
//! stripped — exactly the state a v0.23 store is in at rest. Checking out
//! and compiling the old build inside the test suite was the alternative,
//! and it buys nothing this does not: the same source revision defines both
//! encoders, and the committed fixture already pins the old build's actual
//! output. What the synthetic path adds is scale and coverage (blobs, pins,
//! WAL, erasure records) that no committed fixture could carry without
//! megabytes of opaque bytes in the repository.
//!
//! The downgraded fixtures deliberately keep their v7-era `.rollup`
//! sidecars. Their bindings name the v7 byte lengths, so against the
//! downgraded segments they are stale — self-invalidating, never consulted
//! before migration replaces them with freshly bound ones. A real v6 store's
//! sidecars would be validly bound instead; nothing in the migrator reads a
//! sidecar either way, so the difference is unobservable, and the pr50
//! fixture covers the real-sidecar case.
//!
//! # The aimed-kill pattern
//!
//! The crash matrix follows `tests/durability.rs`: SIGKILL on OBSERVED
//! on-disk signals, never on a stopwatch. Each matrix point is an on-disk
//! state the migration passes through — k of n segments carrying the v7
//! version word, the last segment converted while the big blob still opens
//! with raw bytes, a pin's files all v7 while its manifest still carries the
//! v6-era digests, the pin manifest rewritten while `CURRENT` still names
//! the old generation — and the fixture is shaped so each window is wide
//! (a deliberately large blob and pin put seconds of hashing inside them).
//! The poller kills the child the moment it observes the state; a scenario
//! that misses its window (migration outran the poll) is retried on a fresh
//! copy and fails loudly rather than passing vacuously.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::{Config, Durability, Span, SpanFilter, Store};

// ------------------------------------------------------------------ plumbing

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("traza-mig-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Recursive copy, skipping the LOCK file (a copied lock would name this
/// process as a live owner).
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("copy dir");
    for entry in fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if name.to_string_lossy() == "LOCK" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// The declared format version of a segment file, read exactly as the
/// migrator's trigger reads it: magic then the u16 version word.
fn seg_version(path: &Path) -> Option<u16> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 10 || &bytes[..8] != b"TRAZASEG" {
        return None;
    }
    Some(u16::from_le_bytes([bytes[8], bytes[9]]))
}

/// Paths of every `segment-*.seg` directly under `dir`, sorted.
fn segment_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("segment-") && name.ends_with(".seg")
            })
        })
        .collect();
    paths.sort();
    paths
}

/// Paths of every payload blob under `<dir>/payloads`, sorted.
fn blob_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let root = dir.join("payloads");
    if let Ok(shards) = fs::read_dir(&root) {
        for shard in shards.filter_map(|entry| entry.ok()) {
            if !shard.file_type().expect("type").is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())
                .expect("shard")
                .filter_map(|e| e.ok())
            {
                if entry.file_type().expect("type").is_file() {
                    paths.push(entry.path());
                }
            }
        }
    }
    paths.sort();
    paths
}

fn starts_with_blob_magic(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes.len() >= 8 && &bytes[..8] == b"TRZBLOB1")
}

fn mtimes_of(paths: &[PathBuf]) -> Vec<(PathBuf, SystemTime)> {
    paths
        .iter()
        .map(|path| {
            (
                path.clone(),
                fs::metadata(path)
                    .expect("metadata")
                    .modified()
                    .expect("mtime"),
            )
        })
        .collect()
}

/// Deterministic incompressible-for-LZ4 text (64-symbol alphabet, no long
/// repeats), so blocks exercise the raw-passthrough flag and blobs land on
/// codec 0.
fn pseudo_text(seed: u64, len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = seed | 1;
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(CHARS[(state >> 33) as usize % CHARS.len()] as char);
    }
    out
}

// ---------------------------------------------------------------- v6 writer
//
// A byte-faithful copy of the v6 ENCODER — the frozen decoder's inverse —
// from `src/segment.rs` as of commit 5f23172, cut down to what the
// downgrader needs: the record encoding, the offset table, the trace and
// attribute indexes, and the empty content prologue (a v6 store written with
// content indexing off, a real configuration; migration rebuilds the index
// under the current store's configuration either way, which one test below
// proves by getting content answers back from a fixture that had none).
mod v6 {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use traza::hash::{hash_attribute, Hash128};

    const MAGIC: &[u8; 8] = b"TRAZASEG";
    const VERSION: u16 = 6;
    const HEADER_LEN: usize = 104;
    /// v6 content prologue constants: 128 records per block, 4 hashes.
    const CONTENT_BLOCK_RECORDS: u32 = 128;

    pub struct Record {
        pub timestamp: u64,
        pub trace_id: String,
        pub attributes: BTreeMap<String, String>,
        pub payload: Vec<u8>,
    }

    pub fn encode(records: &[Record]) -> Vec<u8> {
        let mut order: Vec<usize> = (0..records.len()).collect();
        order.sort_by_key(|index| records[*index].timestamp);
        let records: Vec<&Record> = order.into_iter().map(|index| &records[index]).collect();

        let mut record_region = Vec::new();
        let mut offsets = Vec::with_capacity(records.len());
        let mut trace_index: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let attribute_keys: Vec<String> = records
            .iter()
            .flat_map(|record| record.attributes.keys().cloned())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        let key_ids: HashMap<&str, u32> = attribute_keys
            .iter()
            .enumerate()
            .map(|(id, key)| (key.as_str(), id as u32))
            .collect();
        let mut attribute_index: BTreeMap<(u32, Hash128), Vec<u64>> = BTreeMap::new();

        for record in &records {
            let offset = record_region.len() as u64;
            offsets.push(offset);
            put_u64(&mut record_region, record.timestamp);
            put_u32(&mut record_region, record.trace_id.len() as u32);
            put_u32(&mut record_region, record.attributes.len() as u32);
            put_u32(&mut record_region, record.payload.len() as u32);
            put_u32(&mut record_region, 0);
            record_region.extend_from_slice(record.trace_id.as_bytes());
            for (key, value) in &record.attributes {
                put_len_bytes(&mut record_region, key.as_bytes());
                put_len_bytes(&mut record_region, value.as_bytes());
            }
            record_region.extend_from_slice(&record.payload);
            trace_index
                .entry(record.trace_id.clone())
                .or_default()
                .push(offset);
            for (key, value) in &record.attributes {
                let id = *key_ids.get(key.as_str()).expect("key in dictionary");
                attribute_index
                    .entry((id, hash_attribute(key, value)))
                    .or_default()
                    .push(offset);
            }
        }

        let mut offset_region = Vec::with_capacity(offsets.len() * 8);
        for offset in &offsets {
            put_u64(&mut offset_region, *offset);
        }

        // Trace index: v6's string index without values.
        let mut trace_region = Vec::new();
        put_u32(&mut trace_region, trace_index.len() as u32);
        for (key, postings) in &trace_index {
            put_len_bytes(&mut trace_region, key.as_bytes());
            put_u32(&mut trace_region, postings.len() as u32);
            for offset in postings {
                put_u64(&mut trace_region, *offset);
            }
        }

        // Attribute index: key dictionary, then digest-keyed postings.
        let mut attribute_region = Vec::new();
        put_u32(&mut attribute_region, attribute_keys.len() as u32);
        for key in &attribute_keys {
            put_len_bytes(&mut attribute_region, key.as_bytes());
        }
        put_u32(&mut attribute_region, attribute_index.len() as u32);
        for ((key_id, digest), postings) in &attribute_index {
            put_u32(&mut attribute_region, *key_id);
            attribute_region.extend_from_slice(digest.as_bytes());
            put_u32(&mut attribute_region, postings.len() as u32);
            for offset in postings {
                put_u64(&mut attribute_region, *offset);
            }
        }

        // Empty content prologue: zero blocks, "no content index".
        let mut content_region = Vec::with_capacity(32);
        put_u32(&mut content_region, 0);
        put_u32(&mut content_region, CONTENT_BLOCK_RECORDS);
        put_u32(&mut content_region, 0);
        put_u32(&mut content_region, traza::content::HASH_COUNT);
        put_u64(&mut content_region, 0);
        put_u64(&mut content_region, 0);

        let records_offset = HEADER_LEN as u64;
        let offsets_offset = records_offset + record_region.len() as u64;
        let trace_index_offset = offsets_offset + offset_region.len() as u64;
        let attribute_index_offset = trace_index_offset + trace_region.len() as u64;
        let content_index_offset = attribute_index_offset + attribute_region.len() as u64;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        put_u16(&mut bytes, VERSION);
        put_u16(&mut bytes, HEADER_LEN as u16);
        put_u32(&mut bytes, 0);
        put_u64(&mut bytes, records.len() as u64);
        put_u64(&mut bytes, records_offset);
        put_u64(&mut bytes, record_region.len() as u64);
        put_u64(&mut bytes, offsets_offset);
        put_u64(&mut bytes, offset_region.len() as u64);
        put_u64(&mut bytes, trace_index_offset);
        put_u64(&mut bytes, trace_region.len() as u64);
        put_u64(&mut bytes, attribute_index_offset);
        let (min_ts, max_ts) = records.iter().fold((u64::MAX, 0_u64), |(lo, hi), record| {
            (lo.min(record.timestamp), hi.max(record.timestamp))
        });
        put_u64(&mut bytes, min_ts);
        put_u64(&mut bytes, max_ts);
        put_u64(&mut bytes, content_index_offset);
        assert_eq!(bytes.len(), HEADER_LEN, "v6 header layout");
        bytes.extend_from_slice(&record_region);
        bytes.extend_from_slice(&offset_region);
        bytes.extend_from_slice(&trace_region);
        bytes.extend_from_slice(&attribute_region);
        bytes.extend_from_slice(&content_region);
        bytes
    }

    fn put_len_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
        put_u32(out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
    fn put_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn put_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn put_u64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

// ------------------------------------------------------------ the downgrader

/// Re-derives the v6 attribute list from a record's payload, exactly as the
/// v6 seal derived it from the span (`span_to_record` before v7): user
/// attributes minus NUL-prefixed keys, canonicalized to JSON text, plus the
/// reserved `\0service`/`\0name`/`\0tenant` entries carrying raw text.
fn v6_attributes(payload: &[u8]) -> BTreeMap<String, String> {
    let value: Value = serde_json::from_slice(payload).expect("payload parses as JSON");
    let object = value.as_object().expect("span object");
    let mut attributes = BTreeMap::new();
    if let Some(map) = object.get("attributes").and_then(Value::as_object) {
        for (key, item) in map {
            if !key.starts_with('\u{0}') {
                attributes.insert(key.clone(), serde_json::to_string(item).expect("canonical"));
            }
        }
    }
    let raw = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    attributes.insert("\u{0}service".to_owned(), raw("service"));
    attributes.insert("\u{0}name".to_owned(), raw("name"));
    let tenant = raw("$tenant");
    if !tenant.is_empty() {
        attributes.insert("\u{0}tenant".to_owned(), tenant);
    }
    attributes
}

/// Rewrites one v7 segment file as v6, through the frozen encoder above.
///
/// In place (`fs::write`), NOT temp + rename, and deliberately: a pin is a
/// hard-link farm, so writing through the shared inode downgrades the live
/// segment and its pinned twin together — exactly the sharing a real
/// pre-migration pin has with its live store. A file that is already v6 is
/// the pinned twin seen the second time and is left alone.
fn downgrade_segment(path: &Path) {
    if seg_version(path) == Some(6) {
        return;
    }
    let seg = traza::segment::Segment::open(path).expect("open v7 segment");
    let mut records = Vec::with_capacity(seg.len());
    for ordinal in 0..seg.len() {
        let record = seg
            .record(ordinal)
            .expect("record decodes")
            .expect("ordinal in range");
        records.push(v6::Record {
            timestamp: record.timestamp(),
            trace_id: record.trace_id().to_owned(),
            attributes: v6_attributes(record.payload()),
            payload: record.payload().to_vec(),
        });
    }
    drop(seg);
    fs::write(path, v6::encode(&records)).expect("write v6 segment");
}

/// Rewrites every blob under `<base>/payloads` as its raw content — the v6
/// blob format IS the content, whose bytes hash to the file name.
fn downgrade_blobs(base: &Path, contents: &HashMap<String, Vec<u8>>) {
    for path in blob_paths(base) {
        let stem = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned();
        let content = contents
            .get(&stem)
            .unwrap_or_else(|| panic!("fixture bug: unknown blob {stem}"));
        fs::write(&path, content).expect("write raw blob");
    }
}

/// Strips the format declaration from a manifest and re-digests every listed
/// file over the (downgraded) bytes beside it, leaving exactly what a v0.23
/// store's manifest holds at rest.
fn downgrade_manifest(manifest_path: &Path, base: &Path) {
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(manifest_path).expect("manifest")).expect("json");
    let object = manifest.as_object_mut().expect("manifest object");
    object.remove("segment_format");
    for file in object
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .expect("files")
    {
        let relative = file["path"].as_str().expect("path").to_owned();
        let bytes = fs::read(base.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
            .expect("manifested file");
        file["bytes"] = json!(bytes.len());
        file["sha256"] = json!(traza::payload::sha256_hex(&bytes));
        file["modified_unix_ns"] = json!(0);
    }
    fs::write(
        manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("encode"),
    )
    .expect("write manifest");
}

/// Downgrades a closed store directory built by this build into the state a
/// v0.23 store is in at rest: v6 segments, raw blobs, no format declaration,
/// manifests digesting the downgraded bytes — live store and pins alike.
/// The WAL and the append-only logs are untouched; their formats never
/// changed.
fn downgrade_store(dir: &Path, blob_contents: &HashMap<String, Vec<u8>>) {
    for path in segment_paths(dir) {
        downgrade_segment(&path);
    }
    downgrade_blobs(dir, blob_contents);
    let live: u64 = fs::read_to_string(dir.join("CURRENT"))
        .expect("CURRENT")
        .trim()
        .parse()
        .expect("generation id");
    downgrade_manifest(
        &dir.join("generations")
            .join(live.to_string())
            .join("state-manifest.json"),
        dir,
    );
    let pins = dir.join("pins");
    if let Ok(entries) = fs::read_dir(&pins) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            if !entry.file_type().expect("type").is_dir() {
                continue;
            }
            let pin = entry.path();
            for path in segment_paths(&pin) {
                downgrade_segment(&path);
            }
            downgrade_blobs(&pin, blob_contents);
            downgrade_manifest(&pin.join("state-manifest.json"), &pin);
        }
    }
}

// ------------------------------------------------------------- the fixture

struct FixtureSpec {
    segments: usize,
    spans_per_segment: usize,
    /// Length of the inline incompressible padding on every span.
    pad_bytes: usize,
    /// One value of this size on the first span (offloaded when it crosses
    /// the threshold); zero for none.
    big_blob_bytes: usize,
    /// One offloaded value of this size per segment; zero for none.
    small_blob_bytes: usize,
    /// Offload threshold handed to the store.
    payload_threshold: Option<usize>,
    with_pin: bool,
    /// Spans ingested after the last flush: they live only in the WAL.
    wal_tail: usize,
}

fn fixture_config(spec: &FixtureSpec) -> Config {
    Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        compaction: None,
        payload_threshold: spec.payload_threshold,
        ..Config::default()
    }
}

fn fixture_span(batch: usize, index: usize, spec: &FixtureSpec) -> (Span, Vec<(String, String)>) {
    let mut attributes = json!({
        "marker": format!("m{batch}"),
        "needle": format!("needle{batch} shared words"),
    });
    let mut offloaded = Vec::new();
    if spec.pad_bytes > 0 {
        // Alternate incompressible and repetitive padding so compression
        // blocks land on both sides of the raw-passthrough flag.
        let pad = if index % 2 == 0 {
            pseudo_text((batch * 1000 + index) as u64, spec.pad_bytes)
        } else {
            "the quick brown fox jumps over the lazy dog ".repeat(spec.pad_bytes / 44 + 1)
        };
        attributes["pad"] = json!(pad);
    }
    if batch == 0 && index == 0 && spec.big_blob_bytes > 0 {
        let content = pseudo_text(0xB16, spec.big_blob_bytes);
        offloaded.push((
            traza::payload::sha256_hex(content.as_bytes()),
            content.clone(),
        ));
        attributes["big"] = json!(content);
    }
    if index == 1 && spec.small_blob_bytes > 0 {
        let content = pseudo_text(0x5000 + batch as u64, spec.small_blob_bytes);
        offloaded.push((
            traza::payload::sha256_hex(content.as_bytes()),
            content.clone(),
        ));
        attributes["note"] = json!(content);
    }
    let span: Span = serde_json::from_value(json!({
        "trace_id": format!("trace-{batch}"),
        "span_id": format!("s{batch}-{index}"),
        "name": format!("op-{batch}"),
        "service": "svc",
        "start_time_ns": 1_000_000u64 + (batch as u64) * 10_000 + (index as u64) * 10,
        "end_time_ns": 1_000_005u64 + (batch as u64) * 10_000 + (index as u64) * 10,
        "attributes": attributes,
    }))
    .expect("span");
    (span, offloaded)
}

/// Query answers captured as canonical JSON of key-sorted spans, so identity
/// is byte-comparable across opens.
struct Truth {
    everything: Value,
    marker: Value,
    trace: Value,
    content: Value,
    window: Value,
    total: usize,
}

fn canonical(mut spans: Vec<Span>) -> Value {
    spans.sort_by(|left, right| {
        (&left.trace_id, &left.span_id).cmp(&(&right.trace_id, &right.span_id))
    });
    serde_json::to_value(&spans).expect("spans encode")
}

fn capture_truth(store: &Store) -> Truth {
    let everything = store.query(&SpanFilter::default()).expect("query all");
    let total = everything.len();
    Truth {
        everything: canonical(everything),
        marker: canonical(
            store
                .query(&SpanFilter {
                    attributes: vec![("marker".to_owned(), json!("m1"))],
                    ..SpanFilter::default()
                })
                .expect("marker query"),
        ),
        trace: canonical(store.get_trace("trace-0").expect("trace query")),
        content: canonical(
            store
                .query(&SpanFilter {
                    content: Some("needle2".to_owned()),
                    ..SpanFilter::default()
                })
                .expect("content query"),
        ),
        window: canonical(
            store
                .query(&SpanFilter {
                    since_ns: Some(1_010_000),
                    until_ns: Some(1_020_100),
                    ..SpanFilter::default()
                })
                .expect("window query"),
        ),
        total,
    }
}

fn assert_truth(store: &Store, truth: &Truth, context: &str) {
    let now = capture_truth(store);
    assert_eq!(now.total, truth.total, "{context}: span totals");
    assert_eq!(now.everything, truth.everything, "{context}: full scan");
    assert_eq!(now.marker, truth.marker, "{context}: attribute filter");
    assert_eq!(now.trace, truth.trace, "{context}: trace lookup");
    assert_eq!(now.content, truth.content, "{context}: content search");
    assert_eq!(now.window, truth.window, "{context}: time window");
}

/// Builds a store with this build, captures ground truth, and returns the
/// offloaded blob contents by hash. The caller downgrades afterwards.
fn build_store(dir: &Path, spec: &FixtureSpec) -> (Truth, HashMap<String, Vec<u8>>) {
    let store = Store::open(dir, fixture_config(spec)).expect("build store");
    let mut blobs = HashMap::new();
    for batch in 0..spec.segments {
        let mut spans = Vec::new();
        for index in 0..spec.spans_per_segment {
            let (span, offloaded) = fixture_span(batch, index, spec);
            for (hash, content) in offloaded {
                blobs.insert(hash, content.into_bytes());
            }
            spans.push(span);
        }
        store.ingest_batch(spans).expect("ingest batch");
        store.flush().expect("flush seals one segment");
    }
    if spec.with_pin {
        store.pin_generation("premigration").expect("pin");
    }
    for index in 0..spec.wal_tail {
        let span: Span = serde_json::from_value(json!({
            "trace_id": "trace-wal",
            "span_id": format!("w{index}"),
            "name": "op-wal",
            "service": "svc",
            "start_time_ns": 2_000_000u64 + index as u64,
            "end_time_ns": 2_000_001u64 + index as u64,
            "attributes": {"marker": "wal"},
        }))
        .expect("wal span");
        store.ingest(span).expect("wal ingest");
    }
    let truth = capture_truth(&store);
    drop(store);
    (truth, blobs)
}

// ------------------------------------------------------------------- gate 4

#[test]
fn a_v6_store_migrates_at_first_open_with_identical_answers_same_names_and_idempotent_reopen() {
    let dir = test_dir("identity");
    let spec = FixtureSpec {
        segments: 3,
        spans_per_segment: 12,
        pad_bytes: 2_000,
        big_blob_bytes: 200_000,
        small_blob_bytes: 80_000,
        payload_threshold: Some(65_536),
        with_pin: true,
        wal_tail: 5,
    };
    let (truth, blobs) = build_store(&dir, &spec);

    // Snapshot the v7 bytes this build wrote, then downgrade everything.
    // Migration re-derives every record from its payload and re-encodes
    // through the deterministic v7 encoder, so the migrated files must be
    // BYTE-IDENTICAL to what this build wrote in the first place — a
    // stronger form of query identity, asserted alongside it.
    let mut original: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let pin_dir = dir.join("pins").join("premigration");
    for path in segment_paths(&dir)
        .into_iter()
        .chain(blob_paths(&dir))
        .chain(segment_paths(&pin_dir))
        .chain(blob_paths(&pin_dir))
    {
        original.insert(path.clone(), fs::read(&path).expect("snapshot"));
    }
    let original_names: Vec<PathBuf> = segment_paths(&dir);
    let current_before: String = fs::read_to_string(dir.join("CURRENT")).expect("CURRENT");

    downgrade_store(&dir, &blobs);
    for path in segment_paths(&dir)
        .iter()
        .chain(segment_paths(&pin_dir).iter())
    {
        assert_eq!(seg_version(path), Some(6), "downgraded to v6: {path:?}");
    }

    // First open: the migration.
    let started = Instant::now();
    let store = Store::open(&dir, fixture_config(&spec)).expect("migrating open");
    eprintln!(
        "identity fixture migrated in {:?} ({} segments, {} blobs, one pin)",
        started.elapsed(),
        original_names.len(),
        blob_paths(&dir).len(),
    );

    // Query identity, including the WAL tail: `folded_through` was published
    // unchanged, so the frames after it replayed against the migrated store.
    assert_truth(&store, &truth, "after migration");
    assert_eq!(
        store
            .query(&SpanFilter {
                attributes: vec![("marker".to_owned(), json!("wal"))],
                ..SpanFilter::default()
            })
            .expect("wal query")
            .len(),
        spec.wal_tail,
        "WAL frames after folded_through replayed against the migrated store"
    );

    // The post-migration generation proves its manifest: intact, and both
    // the pre-existing pin and a pin taken right now verify — the check that
    // catches a completion checkpoint carrying digests of replaced bytes.
    let live = store.live_generation();
    assert!(
        store.verify_generation(live).expect("verify").is_empty(),
        "the completion checkpoint's generation verifies clean"
    );
    assert!(
        store
            .verify_pin("premigration")
            .expect("verify pin")
            .is_empty(),
        "the migrated pre-existing pin verifies against its rewritten manifest"
    );
    drop(store);

    // Names preserved, and every migrated file byte-identical to the v7
    // encoding this build produced originally.
    assert_eq!(
        segment_paths(&dir),
        original_names,
        "segment names are preserved — path order IS recency order"
    );
    for (path, bytes) in &original {
        assert_eq!(
            &fs::read(path).expect("migrated file"),
            bytes,
            "migrated bytes equal the original v7 encoding: {path:?}"
        );
    }
    let current_after = fs::read_to_string(dir.join("CURRENT")).expect("CURRENT");
    assert_ne!(
        current_before.trim(),
        current_after.trim(),
        "migration published a completion checkpoint"
    );
    let live: u64 = current_after.trim().parse().expect("generation id");
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            dir.join("generations")
                .join(live.to_string())
                .join("state-manifest.json"),
        )
        .expect("manifest"),
    )
    .expect("json");
    assert_eq!(
        manifest["segment_format"],
        json!(7),
        "the completion checkpoint declares the store format"
    );

    // Second open: idempotent, no work. File mtimes and CURRENT both hold.
    let watched: Vec<PathBuf> = original.keys().cloned().collect();
    let before = mtimes_of(&watched);
    let store = Store::open(&dir, fixture_config(&spec)).expect("second open");
    assert_eq!(
        fs::read_to_string(dir.join("CURRENT"))
            .expect("CURRENT")
            .trim(),
        current_after.trim(),
        "a second open publishes nothing"
    );
    assert_eq!(
        mtimes_of(&watched),
        before,
        "a second open rewrites no segment and no blob"
    );
    assert_truth(&store, &truth, "after idempotent reopen");

    // Gate 4's verify-at-pin: a pin taken immediately after migration.
    store
        .pin_generation("post-migration")
        .expect("pin after migration");
    assert!(
        store
            .verify_pin("post-migration")
            .expect("verify")
            .is_empty(),
        "a pin taken immediately after migration passes verify-at-pin"
    );
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------- crash matrix

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// Spawns and waits for the listening line — which arrives only after
    /// `Store::open`, so this rides out a full migration.
    fn spawn(data_dir: &Path) -> Self {
        let mut child = Self::command(data_dir)
            .spawn()
            .expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines.next().expect("port line").expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    /// Spawns WITHOUT waiting: the caller is going to kill this process
    /// mid-migration, long before it would print a port.
    fn spawn_headless(data_dir: &Path) -> Child {
        Self::command(data_dir)
            .spawn()
            .expect("spawns traza-server")
    }

    fn command(data_dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("wal")
            .arg("--flush-spans")
            .arg("1000000")
            .env_remove("TRAZA_TOKENS")
            .stderr(Stdio::piped());
        command
    }

    fn kill_hard(&mut self) {
        self.child.kill().expect("kill");
        self.child.wait().expect("reap");
    }

    fn request(&self, method: &str, target: &str) -> (u16, Value) {
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", self.port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 100 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect: {error}"),
                }
            }
        };
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let payload = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        (status, payload)
    }
}

/// One matrix point: a predicate over the observable on-disk state. The
/// poller kills the child the moment the predicate first holds.
struct MatrixPoint {
    name: &'static str,
    hit: Box<dyn Fn(&Observed) -> bool>,
}

struct Observed {
    live_v7: usize,
    live_total: usize,
    live_blob_v7: bool,
    pin_v7: usize,
    pin_total: usize,
    pin_blob_v7: bool,
    pin_manifest_rewritten: bool,
    current_moved: bool,
}

fn observe(dir: &Path, template: &CrashTemplate) -> Observed {
    let live: Vec<Option<u16>> = template
        .segment_names
        .iter()
        .map(|name| seg_version(&dir.join(name)))
        .collect();
    let pin = dir.join("pins").join("premigration");
    let pin_versions: Vec<Option<u16>> = template
        .segment_names
        .iter()
        .map(|name| seg_version(&pin.join(name)))
        .collect();
    Observed {
        live_v7: live.iter().filter(|v| **v == Some(7)).count(),
        live_total: live.len(),
        live_blob_v7: starts_with_blob_magic(&dir.join(&template.big_blob_relative)),
        pin_v7: pin_versions.iter().filter(|v| **v == Some(7)).count(),
        pin_total: pin_versions.len(),
        pin_blob_v7: starts_with_blob_magic(&pin.join(&template.big_blob_relative)),
        pin_manifest_rewritten: fs::read(pin.join("state-manifest.json"))
            .is_ok_and(|bytes| bytes != template.pin_manifest),
        current_moved: fs::read_to_string(dir.join("CURRENT"))
            .is_ok_and(|current| current.trim() != template.current),
    }
}

struct CrashTemplate {
    dir: PathBuf,
    truth_total: usize,
    marker_count: usize,
    segment_names: Vec<String>,
    big_blob_relative: PathBuf,
    pin_manifest: Vec<u8>,
    current: String,
}

fn build_crash_template() -> CrashTemplate {
    let dir = test_dir("crash-template");
    let spec = FixtureSpec {
        segments: 6,
        spans_per_segment: 40,
        pad_bytes: 18_000,
        big_blob_bytes: 12_000_000,
        small_blob_bytes: 100_000,
        payload_threshold: Some(65_536),
        with_pin: true,
        wal_tail: 20,
    };
    let (truth, blobs) = build_store(&dir, &spec);
    let big_hash = blobs
        .iter()
        .find(|(_, content)| content.len() == 12_000_000)
        .map(|(hash, _)| hash.clone())
        .expect("big blob recorded");
    downgrade_store(&dir, &blobs);
    let segment_names = segment_paths(&dir)
        .iter()
        .map(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let big_blob_relative = Path::new("payloads")
        .join(&big_hash[..2])
        .join(format!("{big_hash}.bin"));
    let pin_manifest = fs::read(
        dir.join("pins")
            .join("premigration")
            .join("state-manifest.json"),
    )
    .expect("pin manifest");
    let current = fs::read_to_string(dir.join("CURRENT"))
        .expect("CURRENT")
        .trim()
        .to_owned();
    CrashTemplate {
        dir,
        truth_total: truth.total,
        marker_count: spec.spans_per_segment,
        segment_names,
        big_blob_relative,
        pin_manifest,
        current,
    }
}

/// Runs one aimed kill: spawn the migrating server, poll the disk, SIGKILL
/// the instant the point's predicate holds. Returns false when migration
/// completed before the predicate was ever observed (the caller retries).
fn kill_at(dir: &Path, template: &CrashTemplate, point: &MatrixPoint) -> bool {
    let mut child = Server::spawn_headless(dir);
    let deadline = Instant::now() + Duration::from_secs(300);
    // `Some(Some(state))`: the point was observed. `Some(None)`: migration
    // completed under the poller — the window was missed. `None`: timed out.
    // One kill-and-reap point for every path, so no exit leaves a zombie.
    let outcome = loop {
        let observed = observe(dir, template);
        if (point.hit)(&observed) {
            break Some(Some(observed));
        }
        if observed.current_moved {
            break Some(None);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    child.kill().expect("kill");
    child.wait().expect("reap");
    match outcome {
        Some(Some(observed)) => {
            eprintln!(
                "matrix point {:?}: killed at live {}/{} v7, live blob v7 {}, pin {}/{} v7, \
                 pin blob v7 {}, pin manifest rewritten {}, CURRENT moved {}",
                point.name,
                observed.live_v7,
                observed.live_total,
                observed.live_blob_v7,
                observed.pin_v7,
                observed.pin_total,
                observed.pin_blob_v7,
                observed.pin_manifest_rewritten,
                observed.current_moved,
            );
            true
        }
        Some(None) => false,
        None => panic!(
            "matrix point {:?} was never observed and migration never finished",
            point.name
        ),
    }
}

/// After a kill: a fresh server finishes the migration on open, and the
/// migrated store proves itself over HTTP — `/v1/verify` intact, the span
/// counts, and a freshly taken pin that verifies (`POST /v1/backups/…`).
fn respawn_and_verify(dir: &Path, template: &CrashTemplate, label: &str) {
    let mut server = Server::spawn(dir);
    let (status, body) = server.request("GET", "/v1/verify");
    assert_eq!(status, 200, "{label}: verify answers: {body}");
    assert_eq!(
        body["intact"],
        json!(true),
        "{label}: the post-migration generation verifies clean: {body}"
    );
    let (status, body) = server.request("GET", "/v1/spans?limit=1000000");
    assert_eq!(status, 200, "{label}: spans answer");
    assert_eq!(
        body["spans"].as_array().map(Vec::len).unwrap_or(0),
        template.truth_total,
        "{label}: every span survived the migration crash"
    );
    let (status, body) = server.request("GET", "/v1/spans?attr.marker=m3&limit=1000000");
    assert_eq!(status, 200, "{label}: attribute query answers");
    assert_eq!(
        body["spans"].as_array().map(Vec::len).unwrap_or(0),
        template.marker_count,
        "{label}: attribute-filtered answers survived"
    );
    // Name preservation, checked before the pin below checkpoints and seals
    // the WAL tail into a new segment.
    let names: Vec<String> = segment_paths(dir)
        .iter()
        .map(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names, template.segment_names,
        "{label}: segment names preserved"
    );
    let (status, body) = server.request("POST", &format!("/v1/backups/pin-{label}"));
    assert_eq!(
        status, 201,
        "{label}: a pin taken immediately after migration verifies: {body}"
    );
    assert_eq!(
        body["verified"],
        json!(true),
        "{label}: pin verified: {body}"
    );
    server.kill_hard();

    // The pre-existing pin, through the engine: its manifest rewrite (redone
    // by resume where the kill landed between file pass and rewrite) must
    // hold digests of the migrated bytes.
    let store = Store::open(
        dir,
        Config {
            flush_spans: 1_000_000,
            durability: Durability::Wal,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("post-verification open");
    assert!(
        store
            .verify_pin("premigration")
            .expect("verify pin")
            .is_empty(),
        "{label}: the migrated pre-existing pin verifies clean"
    );
    drop(store);
}

#[test]
fn migration_survives_sigkill_at_each_matrix_point() {
    let template = build_crash_template();

    // Scenario 0: a clean, uninterrupted migration of the full fixture —
    // also the migration wall-time measurement for the biggest fixture this
    // suite builds.
    {
        let dir = test_dir("crash-clean");
        copy_tree(&template.dir, &dir);
        let started = Instant::now();
        respawn_and_verify(&dir, &template, "clean");
        eprintln!(
            "full migration of the crash-matrix fixture (6 segments × ~740 KiB inline text, \
             a 12 MB blob, 6 small blobs, one full pin, 20 WAL frames) took {:?} \
             including server startup",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    let points: Vec<MatrixPoint> = vec![
        MatrixPoint {
            name: "after k of n segment renames",
            hit: Box::new(|o| o.live_v7 >= 2 && o.live_v7 < o.live_total),
        },
        MatrixPoint {
            name: "between the segment pass and the blob pass",
            hit: Box::new(|o| o.live_v7 == o.live_total && !o.live_blob_v7),
        },
        MatrixPoint {
            name: "between a pin's file pass and its manifest rewrite",
            hit: Box::new(|o| {
                o.pin_v7 == o.pin_total && o.pin_blob_v7 && !o.pin_manifest_rewritten
            }),
        },
        MatrixPoint {
            name: "between the blob/pin passes and the completion checkpoint",
            hit: Box::new(|o| o.pin_manifest_rewritten && !o.current_moved),
        },
    ];

    for (index, point) in points.iter().enumerate() {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let dir = test_dir(&format!("crash-{index}"));
            copy_tree(&template.dir, &dir);
            if kill_at(&dir, &template, point) {
                respawn_and_verify(&dir, &template, &format!("point-{index}-try-{attempts}"));
                let _ = fs::remove_dir_all(&dir);
                break;
            }
            let _ = fs::remove_dir_all(&dir);
            assert!(
                attempts < 3,
                "matrix point {:?} was missed {attempts} times — migration outran the \
                 poller; grow the fixture",
                point.name
            );
        }
    }
    let _ = fs::remove_dir_all(&template.dir);
}

// ---------------------------------------------------- erasure across migration

#[test]
fn erasure_rides_across_the_migration_pending_settles_and_settled_receipts_re_derive() {
    let dir = test_dir("erasure");
    let config = Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        compaction: None,
        ..Config::default()
    };
    let span = |trace: &str, id: &str| -> Span {
        serde_json::from_value(json!({
            "trace_id": trace, "span_id": id, "name": format!("op-{id}"),
            "service": "svc", "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
            "attributes": {"marker": trace},
        }))
        .expect("span")
    };

    // A settled erasure BEFORE migration, plus data that stays.
    let settled_id;
    {
        let store = Store::open(&dir, config.clone()).expect("build store");
        store
            .ingest_batch(vec![
                span("t-kept", "k0"),
                span("t-kept", "k1"),
                span("t-erased", "e0"),
                span("t-erased", "e1"),
            ])
            .expect("ingest");
        store.flush().expect("flush");
        let status = store
            .erase(traza::erasure::Subject::Trace {
                trace_id: "t-erased".to_owned(),
                tenant: String::new(),
            })
            .expect("erase settles");
        assert!(status.settle.is_some(), "the pre-migration erasure settled");
        settled_id = status.erase.id;
        store
            .ingest_batch(vec![span("t-pending", "p0"), span("t-pending", "p1")])
            .expect("ingest pending subject");
        store.flush().expect("flush");
        drop(store);
    }

    // Downgrade, then plant the crash state: an erasure whose intent was
    // recorded and whose purge never ran — exactly what a v6-era crash
    // between `begin` and settle leaves behind.
    downgrade_store(&dir, &HashMap::new());
    let pending = json!({
        "op": "erase", "schema": 1, "id": settled_id + 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "t-pending"},
        "span_keys": [["t-pending", "p0"], ["t-pending", "p1"]], "payload_refs": [],
    });
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("tombstones.jsonl"))
        .expect("tombstone log");
    writeln!(log, "{pending}").expect("plant pending erasure");
    drop(log);

    // Open migrates first; the pending erasure masks its subject against the
    // migrated store, and resume settles it there.
    let store = Store::open(&dir, config).expect("migrating open");
    assert!(
        store
            .get_trace("t-pending")
            .expect("masked lookup")
            .is_empty(),
        "the pending erasure masks its subject immediately after migration"
    );
    assert_eq!(
        store.resume_erasures().expect("resume"),
        1,
        "resume settles the pending erasure against the migrated store"
    );
    assert!(
        store.get_trace("t-pending").expect("lookup").is_empty(),
        "the resumed purge removed the subject"
    );
    assert!(
        store.get_trace("t-erased").expect("lookup").is_empty(),
        "the pre-migration erasure still holds"
    );
    assert_eq!(
        store.get_trace("t-kept").expect("lookup").len(),
        2,
        "unrelated data survives migration and both erasures"
    );

    // The settled pre-migration receipt cites generations whose digests died
    // with the rewrite — the documented loss — but the FINDING re-derives:
    // re-running verification against the migrated store still proves the
    // erasure.
    let receipt = store.verify_erasure(settled_id).expect("re-derive receipt");
    assert!(receipt.settled, "the old settle record still reads");
    assert_eq!(
        receipt.result,
        "erased",
        "the pre-migration erasure re-verifies against the migrated store:\n{}",
        receipt.render_text()
    );
    let receipt = store
        .verify_erasure(settled_id + 1)
        .expect("resumed receipt");
    assert_eq!(
        receipt.result,
        "erased",
        "the resumed erasure verifies too:\n{}",
        receipt.render_text()
    );
    assert!(
        store
            .verify_generation(store.live_generation())
            .expect("verify")
            .is_empty(),
        "the store verifies clean after migration plus a resumed erasure"
    );
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

// ------------------------------------------------------- blob classification

#[test]
fn the_blob_pass_is_a_three_way_classification_and_refuses_to_launder_corrupt_bytes() {
    let dir = test_dir("three-way");
    let spec = FixtureSpec {
        segments: 1,
        spans_per_segment: 4,
        pad_bytes: 0,
        big_blob_bytes: 2_000,
        small_blob_bytes: 1_500,
        payload_threshold: Some(512),
        with_pin: false,
        wal_tail: 0,
    };
    let (_, blobs) = build_store(&dir, &spec);
    let paths = blob_paths(&dir);
    assert_eq!(paths.len(), 2, "two offloaded values, two blobs");

    // Segments stay v7; only the declaration is stripped and one blob
    // downgraded — this drives the RESUME trigger (all segments v7, no
    // manifest declaration) rather than the full-migration one, so both
    // trigger arms see the three-way rule.
    let downgraded = &paths[0]; // becomes a raw v6 blob
    let untouched = &paths[1]; // stays a valid v7 blob
    let stem = downgraded
        .file_stem()
        .expect("stem")
        .to_string_lossy()
        .into_owned();
    fs::write(downgraded, &blobs[&stem]).expect("downgrade one blob");
    let untouched_bytes = fs::read(untouched).expect("v7 blob bytes");
    let live: u64 = fs::read_to_string(dir.join("CURRENT"))
        .expect("CURRENT")
        .trim()
        .parse()
        .expect("generation");
    downgrade_manifest(
        &dir.join("generations")
            .join(live.to_string())
            .join("state-manifest.json"),
        &dir,
    );

    // Plant the third case: a content-addressed name whose bytes are neither
    // a valid v7 blob nor bytes hashing to the name.
    let corrupt_shard = dir.join("payloads").join("ff");
    fs::create_dir_all(&corrupt_shard).expect("shard");
    let corrupt = corrupt_shard.join(format!("{}.bin", "f".repeat(64)));
    fs::write(&corrupt, b"matches neither format").expect("plant corrupt blob");

    let error = Store::open(&dir, fixture_config(&spec))
        .err()
        .expect("a corrupt blob refuses the migration")
        .to_string();
    assert!(
        error.contains(&corrupt.display().to_string()),
        "the refusal names the corrupt file: {error}"
    );
    assert!(
        error.contains("refuses to rewrite"),
        "the refusal says what it will not do: {error}"
    );

    // First case untouched, second rewritten, third untouched bytes.
    assert_eq!(
        fs::read(untouched).expect("v7 blob"),
        untouched_bytes,
        "a blob that already passes v7 validation is left byte-for-byte alone"
    );
    assert!(
        starts_with_blob_magic(downgraded),
        "a raw blob whose bytes hash to its name was rewritten as TRZBLOB1"
    );
    assert_eq!(
        fs::read(&corrupt).expect("corrupt file"),
        b"matches neither format",
        "the corrupt file was not rewritten — laundering refused"
    );

    // Remove the corrupt file: the next open finishes what the refusal
    // interrupted, exactly like any other resume.
    fs::remove_file(&corrupt).expect("operator removes the corrupt file");
    let _ = fs::remove_dir_all(&corrupt_shard);
    let store = Store::open(&dir, fixture_config(&spec)).expect("resume finishes");
    assert!(
        store
            .verify_generation(store.live_generation())
            .expect("verify")
            .is_empty(),
        "the completion checkpoint digests the converted blobs"
    );
    drop(store);

    // And a further open does nothing at all.
    let watched = blob_paths(&dir);
    let before = mtimes_of(&watched);
    let store = Store::open(&dir, fixture_config(&spec)).expect("idempotent open");
    assert_eq!(mtimes_of(&watched), before, "no blob is touched twice");
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_segment_head_beside_v6_segments_is_refused_by_name() {
    // Review found the trigger scan stepping aside for ANY unreadable head —
    // and `load_segments` then aborts on the FIRST unreadable segment in
    // path order, so in a v6 store a healthy v6 segment sorting earlier drew
    // an UnsupportedVersion refusal with legacy-reader advice while the
    // actually-damaged file was never named. A v6 store with an unreadable
    // segment head must refuse by naming exactly that file.
    let dir = test_dir("unreadable-head");
    let spec = FixtureSpec {
        segments: 2,
        spans_per_segment: 4,
        pad_bytes: 0,
        big_blob_bytes: 0,
        small_blob_bytes: 0,
        payload_threshold: None,
        with_pin: false,
        wal_tail: 0,
    };
    let (_truth, blobs) = build_store(&dir, &spec);
    downgrade_store(&dir, &blobs);

    // An interrupted copy left a five-byte segment file that sorts AFTER the
    // healthy v6 segments — the order that misdirected the old refusal.
    let paths = segment_paths(&dir);
    assert!(paths.len() >= 2, "the fixture holds several segments");
    let damaged = paths.last().expect("a segment").clone();
    fs::write(&damaged, b"TRAZA").expect("truncate the head");

    let error = Store::open(&dir, fixture_config(&spec))
        .err()
        .expect("a v6 store with an unreadable head must not open")
        .to_string();
    assert!(
        error.contains(&damaged.display().to_string()),
        "the refusal names the damaged file: {error}"
    );
    assert!(
        error.contains("do not read as v6 or v7"),
        "the refusal says what is wrong with it: {error}"
    );
    assert!(
        !error.contains("formats 2 through 5"),
        "no misdirected legacy-reader advice: {error}"
    );

    // Nothing was converted around the damage: the healthy segments are
    // still v6, waiting for the migration the refusal promised.
    for path in &paths[..paths.len() - 1] {
        assert_eq!(seg_version(path), Some(6), "no segment was converted");
    }

    // Resolving the file lets that migration run to completion.
    fs::remove_file(&damaged).expect("operator removes the damaged file");
    let store = Store::open(&dir, fixture_config(&spec)).expect("the migration runs");
    for path in segment_paths(&dir) {
        assert_eq!(
            seg_version(&path),
            Some(7),
            "every remaining segment migrated"
        );
    }
    drop(store);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_digest_moved_pinned_segment_is_validated_in_full_before_reacceptance() {
    // The pin-resume re-validation exists so a crash between a pin's file
    // pass and its manifest rewrite cannot strand v6-era digests. Review
    // found it accepted a moved digest after only the LAZY `Segment::open` —
    // header and index sections, ~10% of the bytes — so a bit-flipped block
    // in a pinned segment was laundered into a rewritten manifest that then
    // verified clean over garbage. The moved digest must be accepted only
    // after the whole file proves itself, block CRCs included.
    let dir = test_dir("pin-launder");
    let spec = FixtureSpec {
        segments: 1,
        spans_per_segment: 8,
        pad_bytes: 2_000,
        big_blob_bytes: 0,
        small_blob_bytes: 0,
        payload_threshold: None,
        with_pin: true,
        wal_tail: 0,
    };
    let (_truth, _blobs) = build_store(&dir, &spec);

    // Flip one byte inside the pinned segment's records region — inside a
    // stored compression block, past everything the lazy open reads. The
    // pin shares the inode with the live segment (a hard-link farm), which
    // is fine here: migration refuses before anything reads the live copy.
    let pin = dir.join("pins").join("premigration");
    let pinned_segment = segment_paths(&pin).pop().expect("the pin holds a segment");
    let mut bytes = fs::read(&pinned_segment).expect("pinned segment bytes");
    let records_offset = u64::from_le_bytes(bytes[24..32].try_into().expect("u64")) as usize;
    let records_len = u64::from_le_bytes(bytes[32..40].try_into().expect("u64")) as usize;
    assert!(records_len > 64, "the fixture has a real records region");
    bytes[records_offset + records_len / 2] ^= 0xff;
    fs::write(&pinned_segment, &bytes).expect("corrupt one stored byte");

    // Strip the live manifest's format declaration: the next open now runs
    // the resume passes, which re-validate every pin — the exact path the
    // laundering lived on.
    let live: u64 = fs::read_to_string(dir.join("CURRENT"))
        .expect("CURRENT")
        .trim()
        .parse()
        .expect("generation id");
    downgrade_manifest(
        &dir.join("generations")
            .join(live.to_string())
            .join("state-manifest.json"),
        &dir,
    );
    let pin_manifest_before =
        fs::read(pin.join("state-manifest.json")).expect("pin manifest before");

    let error = Store::open(&dir, fixture_config(&spec))
        .err()
        .expect("a corrupt digest-moved pinned segment must refuse the resume")
        .to_string();
    assert!(
        error.contains(&pin.display().to_string()),
        "the refusal names the pin: {error}"
    );
    assert!(
        error.contains("does not validate as a v7 segment"),
        "the refusal says why the digest is not re-accepted: {error}"
    );

    // The laundering did not happen: the pin's manifest still carries the
    // honest pre-corruption digests, so restore verification still fails
    // loudly on the damaged file instead of passing over it.
    let pin_manifest_after = fs::read(pin.join("state-manifest.json")).expect("pin manifest after");
    assert_eq!(
        pin_manifest_before, pin_manifest_after,
        "the pin manifest is left alone when the file cannot prove itself"
    );
    let _ = fs::remove_dir_all(&dir);
}
