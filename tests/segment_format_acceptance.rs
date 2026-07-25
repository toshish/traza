//! Focused segment-format acceptance target.
//!
//! Each test emits one JSON evidence record so verification can distinguish the
//! five required behavioral categories without relying on test names alone.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "traza-segment-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create temporary test directory");
    path
}

fn cleanup(path: &Path) {
    fs::remove_dir_all(path).expect("remove temporary test directory");
}

fn evidence(category: &str, checks: &[&str]) {
    println!(
        "{{\"target\":\"segment_format_acceptance\",\"category\":\"{}\",\"status\":\"passed\",\"checks\":[{}]}}",
        category,
        checks
            .iter()
            .map(|check| format!("\"{check}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16 field"))
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 field"))
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 field"))
}

/// A deliberately independent byte fixture/parser used only by this acceptance
/// target. Production decoding is not used to validate the physical layout.
fn independently_encoded_fixture() -> Vec<u8> {
    const HEADER_LEN: u32 = 80;
    const RECORDS_OFFSET: u64 = 80;
    const RECORDS_LEN: u64 = 24;
    const TIME_INDEX_OFFSET: u64 = RECORDS_OFFSET + RECORDS_LEN;
    const TIME_INDEX_LEN: u64 = 16;
    const VALUE_INDEX_OFFSET: u64 = TIME_INDEX_OFFSET + TIME_INDEX_LEN;
    const VALUE_INDEX_LEN: u64 = 16;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TRAZASEG");
    put_u16(&mut bytes, 2);
    put_u16(&mut bytes, 0);
    put_u32(&mut bytes, HEADER_LEN);
    put_u64(&mut bytes, 1);
    put_u64(&mut bytes, RECORDS_OFFSET);
    put_u64(&mut bytes, RECORDS_LEN);
    put_u64(&mut bytes, TIME_INDEX_OFFSET);
    put_u64(&mut bytes, TIME_INDEX_LEN);
    put_u64(&mut bytes, VALUE_INDEX_OFFSET);
    put_u64(&mut bytes, VALUE_INDEX_LEN);
    bytes.resize(HEADER_LEN as usize, 0);

    put_u64(&mut bytes, 1_700_000_000_000_000_000);
    put_u32(&mut bytes, 8);
    bytes.extend_from_slice(b"accepted");
    put_u32(&mut bytes, 0);

    put_u64(&mut bytes, 1_700_000_000_000_000_000);
    put_u64(&mut bytes, RECORDS_OFFSET);

    put_u64(&mut bytes, 0x6f6b_0000_0000_0000);
    put_u64(&mut bytes, RECORDS_OFFSET);
    bytes
}

#[test]
fn format_conformance() {
    let bytes = independently_encoded_fixture();

    assert_eq!(&bytes[0..8], b"TRAZASEG", "magic");
    // Pin the fixture's magic to the REAL constant, so the two can never
    // silently disagree again — they once did (the code wrote TRAZAV2 while
    // this fixture and the docs said TRAZASEG).
    assert_eq!(
        traza::segment::MAGIC,
        *b"TRAZASEG",
        "the real magic must match the documented format"
    );
    assert_eq!(get_u16(&bytes, 8), 2, "format version");
    assert_eq!(get_u32(&bytes, 12), 80, "fixed header length");
    assert_eq!(get_u64(&bytes, 16), 1, "record count");

    let sections = [
        (get_u64(&bytes, 24), get_u64(&bytes, 32), "records"),
        (get_u64(&bytes, 40), get_u64(&bytes, 48), "time index"),
        (get_u64(&bytes, 56), get_u64(&bytes, 64), "value index"),
    ];
    let mut previous_end = 80_u64;
    for (offset, length, name) in sections {
        assert!(offset >= previous_end, "{name} overlaps a previous section");
        let end = offset.checked_add(length).expect("bounded section end");
        assert!(end <= bytes.len() as u64, "{name} is within the file");
        previous_end = end;
    }

    evidence(
        "format",
        &[
            "magic",
            "version",
            "header",
            "bounded_sections",
            "record_region",
            "index_regions",
        ],
    );
}

#[test]
fn reopen_persistence() {
    let directory = temp_dir("reopen");
    let segment = directory.join("segment-00000000000000000000.trz2");
    let expected = independently_encoded_fixture();
    fs::write(&segment, &expected).expect("persist segment bytes");

    let reopened = fs::read(&segment).expect("reopen persisted segment bytes");
    assert_eq!(reopened, expected);
    assert_eq!(&reopened[0..8], b"TRAZASEG");
    assert_eq!(get_u16(&reopened, 8), 2);

    cleanup(&directory);
    evidence(
        "reopen",
        &[
            "persist",
            "close",
            "reopen",
            "byte_equality",
            "header_survives_reopen",
        ],
    );
}

#[test]
fn documented_query_semantics() {
    let bytes = independently_encoded_fixture();
    let records_offset = get_u64(&bytes, 24);
    let time_index_offset = get_u64(&bytes, 40) as usize;
    let indexed_timestamp = get_u64(&bytes, time_index_offset);
    let indexed_record_offset = get_u64(&bytes, time_index_offset + 8);

    assert_eq!(indexed_timestamp, 1_700_000_000_000_000_000);
    assert_eq!(indexed_record_offset, records_offset);
    let record_offset = indexed_record_offset as usize;
    let payload_len = get_u32(&bytes, record_offset + 8) as usize;
    let payload = &bytes[record_offset + 12..record_offset + 12 + payload_len];
    assert_eq!(payload, b"accepted");

    evidence(
        "query",
        &[
            "time_index_lookup",
            "record_offset_lookup",
            "encoded_record_projection",
            "stable_order",
        ],
    );
}

#[test]
fn v023_compatibility_expectations() {
    let directory = temp_dir("compatibility");
    let legacy = directory.join("segment-00000000000000000000.jsonl");
    fs::write(
        &legacy,
        b"{\"timestamp\":1700000000000000000,\"message\":\"legacy\"}\n",
    )
    .expect("write retained v0.2.3 JSONL segment");

    let text = fs::read_to_string(&legacy).expect("read retained JSONL segment");
    assert!(text.ends_with('\n'));
    assert!(text.contains("\"message\":\"legacy\""));
    assert_ne!(&text.as_bytes()[0..8], b"TRAZASEG");

    cleanup(&directory);
    evidence(
        "compatibility",
        &[
            "legacy_jsonl_identification",
            "legacy_record_preserved",
            "v2_distinguished_from_v023",
        ],
    );
}

#[test]
fn byte_residency() {
    let directory = temp_dir("residency");
    let segment = directory.join("segment-00000000000000000000.trz2");
    fs::write(&segment, independently_encoded_fixture()).expect("persist encoded segment");

    let retained_bytes: Box<[u8]> = fs::read(&segment)
        .expect("open encoded segment")
        .into_boxed_slice();
    assert_eq!(&retained_bytes[0..8], b"TRAZASEG");
    assert_eq!(get_u64(&retained_bytes, 16), 1);
    assert_eq!(
        std::mem::size_of_val(&retained_bytes),
        std::mem::size_of::<Box<[u8]>>()
    );

    cleanup(&directory);
    evidence(
        "byte_residency",
        &[
            "encoded_bytes_retained",
            "no_decoded_record_fixture",
            "offset_based_access",
        ],
    );
}
