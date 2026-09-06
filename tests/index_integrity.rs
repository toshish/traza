//! Persisted-index integrity: corruption in exclusion metadata must never
//! yield a silent empty answer.
//!
//! The defect these tests pin was reproduced against v0.24.2 (format v7):
//! flip the last byte of `trace-a` inside the trace-index section of a
//! sealed segment, reopen, and `get_trace("trace-a")` answered ZERO rows
//! while a full scan still found the span and `verify_generation` reported
//! the digest mismatch nobody had asked it about. The trace index is
//! EXCLUSION metadata — an absent entry means "this segment holds nothing
//! for that key" and prunes the segment without a decode — and v7 stored it,
//! the attribute index, the record-offset index, and the content index's
//! bit-sliced rows with no integrity protection at all. Format v8 wraps
//! every metadata section in a checksum and covers the header with its own,
//! so each mutation below must surface as a refusal at reopen, never as a
//! shrunken answer.
//!
//! Shown to fail, per the testing standard: with the wrapper-checksum
//! comparison in `unwrap_section` disabled (`if false &&` — the v7 behavior
//! restored), `a_flipped_trace_index_byte_is_refused_at_reopen` goes red on
//! exactly the baseline shape — reopen accepted, `get_trace` empty — and
//! the rest of the section-mutation tests go red with it; with the header
//! CRC comparison disabled, the header-mutation test goes red. Evidence is
//! recorded in the implementation notes for the v8 release.

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
        "traza-integrity-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("test directory");
    dir
}

fn wal_config() -> Config {
    Config {
        durability: Durability::Wal,
        ..Config::default()
    }
}

/// Builds the one-span store from the defect reproduction: `trace-a/span-a`
/// carrying an indexed attribute and content text, sealed and checkpointed.
fn sealed_store(label: &str) -> (PathBuf, PathBuf) {
    let dir = test_dir(label);
    let store = Store::open(&dir, wal_config()).expect("open");
    let span: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "trace-a", "span_id": "span-a", "name": "op", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": {"marker": "needle", "note": "the antidisestablishment memo"},
    }))
    .expect("span");
    store.ingest_batch(vec![span]).expect("ingest");
    store.flush().expect("flush");
    store.checkpoint().expect("checkpoint");
    drop(store);
    let segment = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("a sealed segment");
    (dir, segment)
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64"))
}

/// The five metadata sections of a v8 segment, as `(offset, length)` read
/// from the header words the format documents.
fn sections(bytes: &[u8]) -> [(&'static str, usize, usize); 5] {
    let directory = (get_u64(bytes, 104) as usize, get_u64(bytes, 112) as usize);
    let offsets = (get_u64(bytes, 40) as usize, get_u64(bytes, 48) as usize);
    let trace = (get_u64(bytes, 56) as usize, get_u64(bytes, 64) as usize);
    let attribute = get_u64(bytes, 72) as usize;
    let content = get_u64(bytes, 96) as usize;
    [
        ("block directory", directory.0, directory.1),
        ("record-offset index", offsets.0, offsets.1),
        ("trace index", trace.0, trace.1),
        ("attribute index", attribute, content - attribute),
        ("content index", content, bytes.len() - content),
    ]
}

/// The contract under mutation: the store must refuse to open (naming
/// corruption), or — if some future change heals instead — still answer the
/// query with the span. A clean empty answer is the defect.
fn assert_never_a_silent_omission(dir: &Path, what: &str) {
    match Store::open(dir, wal_config()) {
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.to_lowercase().contains("corrupt"),
                "{what}: the refusal names corruption: {text}"
            );
        }
        Ok(store) => {
            let by_trace = store.get_trace("trace-a").expect("get_trace");
            assert!(
                by_trace.iter().any(|span| span.span_id == "span-a"),
                "{what}: reopen was accepted and get_trace(trace-a) answered \
                 {} spans while the record is still on disk — the silent \
                 empty answer this format exists to refuse",
                by_trace.len()
            );
            let filtered = store
                .query(&SpanFilter {
                    attributes: vec![("marker".into(), "needle".into())],
                    ..SpanFilter::default()
                })
                .expect("attribute query");
            assert!(
                filtered.iter().any(|span| span.span_id == "span-a"),
                "{what}: an attribute filter silently lost the span"
            );
        }
    }
}

/// The reproduced v0.24.2 defect, byte for byte: mutate the last byte of
/// `trace-a` inside the trace-index section and reopen. On the baseline the
/// store opened and answered empty; under v8 the section checksum refuses
/// the segment at open.
#[test]
fn a_flipped_trace_index_byte_is_refused_at_reopen() {
    let (dir, segment) = sealed_store("trace-index-flip");
    let mut bytes = fs::read(&segment).expect("segment bytes");
    let trace_offset = get_u64(&bytes, 56) as usize;
    let trace_len = get_u64(&bytes, 64) as usize;
    // v8 may store the section compressed, so the mutation targets the last
    // byte of the section rather than a text match; the guarantee under
    // test is byte-level, not field-level.
    let target = trace_offset + trace_len - 1;
    bytes[target] ^= 0x01;
    fs::write(&segment, &bytes).expect("write mutated segment");

    let refusal = Store::open(&dir, wal_config())
        .err()
        .expect("a store with a mutated trace index must not open");
    let text = refusal.to_string();
    assert!(
        text.contains("trace index") && text.contains("checksum"),
        "the refusal names the section and the check: {text}"
    );
    assert!(
        text.contains("segment-"),
        "the refusal names the file: {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Every metadata section, one flipped byte each — mid-section and at both
/// edges of the body — must be a refusal or a served span, never a clean
/// empty answer. This sweeps the whole exclusion surface: block directory
/// (window pruning), record offsets (scan paths), trace index (get_trace),
/// attribute index (filter pruning), content index (word-search pruning).
#[test]
fn every_metadata_section_is_guarded_against_single_byte_mutation() {
    let (reference_dir, reference_segment) = sealed_store("section-sweep-reference");
    let honest = fs::read(&reference_segment).expect("segment bytes");
    let _ = fs::remove_dir_all(&reference_dir);

    for (name, offset, len) in sections(&honest) {
        // First body byte (past the 16-byte wrapper), a middle byte, and
        // the last byte of the section.
        for (position_name, target) in [
            ("first body byte", offset + 16),
            ("middle byte", offset + len / 2),
            ("last byte", offset + len - 1),
        ] {
            let (dir, segment) = sealed_store("section-sweep");
            let mut bytes = fs::read(&segment).expect("segment bytes");
            assert_eq!(
                bytes, honest,
                "the sealed segment is deterministic, so the reference offsets apply"
            );
            bytes[target] ^= 0x01;
            fs::write(&segment, &bytes).expect("write mutated segment");
            assert_never_a_silent_omission(&dir, &format!("{name}, {position_name}"));
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// A flipped header byte — section offsets, record count, codec id, the
/// timestamp range — must be refused by the header CRC (or a deeper check),
/// never misread into wrong pruning. Byte 8..10 (the version word) is
/// excluded: a changed version is a version refusal by design, which the
/// storage suite covers separately.
#[test]
fn a_flipped_header_byte_is_refused_at_reopen() {
    let (reference_dir, reference_segment) = sealed_store("header-sweep-reference");
    let honest = fs::read(&reference_segment).expect("segment bytes");
    let _ = fs::remove_dir_all(&reference_dir);

    // Every header byte the CRC covers except the magic (a foreign-file
    // refusal of its own) and the version word.
    for target in 10..132usize {
        let (dir, segment) = sealed_store("header-sweep");
        let mut bytes = fs::read(&segment).expect("segment bytes");
        assert_eq!(bytes, honest, "deterministic seal");
        bytes[target] ^= 0x01;
        fs::write(&segment, &bytes).expect("write mutated segment");
        assert_never_a_silent_omission(&dir, &format!("header byte {target}"));
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Content search is pruned through the summary filter and the bit-sliced
/// rows; a cleared row bit silently dropped matches under v7. Under v8 the
/// section checksum refuses the segment at open, so the query never sees
/// the doctored filter.
#[test]
fn a_cleared_content_row_cannot_silently_drop_a_word_match() {
    let (dir, segment) = sealed_store("content-row");
    let mut bytes = fs::read(&segment).expect("segment bytes");
    let content_offset = get_u64(&bytes, 96) as usize;
    // Zero every bit-sliced row: the resident summary still admits the
    // query, the row read then denies every block, and under v7 the store
    // answered the word search with a clean empty result. The rows start
    // after the section wrapper (16 bytes), the prologue (32 bytes), the
    // summary filter (bit length declared at prologue byte 16), and the
    // page-checksum table (one CRC-32 per 4096-byte page of rows). This
    // mutation is BEFORE reopen, so the section wrapper is what refuses it;
    // the post-open variant lives in the acceptance suite, where the page
    // table is the guard.
    let body = content_offset + 16;
    let block_count = u64::from(u32::from_le_bytes(
        bytes[body + 8..body + 12].try_into().expect("u32"),
    ));
    let summary_bits = get_u64(&bytes, body + 16) as usize;
    let block_bits = get_u64(&bytes, body + 24);
    let rows_len = block_bits * block_count.div_ceil(8);
    let table_len = (rows_len.div_ceil(4096) * 4) as usize;
    let rows_start = body + 32 + summary_bits / 8 + table_len;
    assert!(rows_start < bytes.len(), "the section has bit-sliced rows");
    for byte in &mut bytes[rows_start..] {
        *byte = 0;
    }
    fs::write(&segment, &bytes).expect("write mutated segment");

    match Store::open(&dir, wal_config()) {
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("content index") && text.contains("checksum"),
                "the refusal names the content index: {text}"
            );
        }
        Ok(store) => {
            // If a future change tolerates the damage, the word must still
            // be found — content pruning may only ever over-approximate.
            let spans = store
                .query(&SpanFilter {
                    content: Some("antidisestablishment".into()),
                    ..SpanFilter::default()
                })
                .expect("content query");
            assert!(
                spans.iter().any(|span| span.span_id == "span-a"),
                "a doctored content row silently dropped a word match"
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}
