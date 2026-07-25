//! Segment-format acceptance target.
//!
//! Every byte examined here comes from the REAL encoder
//! (`traza::segment::encode`), and every behavioral claim is checked through
//! the real reader. The parsing is still done by hand at fixed offsets — that
//! independence is the point of this target — but it parses the layout the
//! engine actually writes.
//!
//! This file previously built its own "independent" byte fixture. Because the
//! fixture was only ever compared against itself and never fed to the reader,
//! it drifted into a format Traza has never written (a u32 header length at
//! offset 12, three sections instead of four, a `.trz2` extension). It passed
//! continuously while asserting nothing about the engine. Hand-parsing is
//! worth keeping; inventing the bytes is not.
//!
//! Each test emits one JSON evidence record so verification can distinguish the
//! behavioral categories without relying on test names alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use traza::segment::{self, RecordInput, Segment, HEADER_LEN, MAGIC, VERSION};

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

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16 field"))
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 field"))
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 field"))
}

fn attributes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The corpus every test in this file encodes. Two traces, overlapping
/// attribute values and distinct payloads, so index lookups have something to
/// discriminate rather than trivially matching everything.
fn corpus() -> Vec<RecordInput> {
    vec![
        RecordInput::new(
            1_700_000_000_000_000_000,
            "trace-a",
            attributes(&[("service", "checkout"), ("status", "ok")]),
            b"first".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_100,
            "trace-b",
            attributes(&[("service", "checkout"), ("status", "error")]),
            b"second".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_200,
            "trace-a",
            attributes(&[("service", "billing"), ("status", "ok")]),
            b"third".to_vec(),
        ),
    ]
}

/// Header field offsets, transcribed from `Header::parse_with_total`. Named
/// here so a layout change breaks this file loudly rather than shifting a
/// magic number silently.
mod offsets {
    pub const VERSION: usize = 8;
    pub const HEADER_LEN: usize = 10;
    pub const RESERVED: usize = 12;
    pub const RECORD_COUNT: usize = 16;
    pub const RECORDS_OFFSET: usize = 24;
    pub const RECORDS_LEN: usize = 32;
    pub const OFFSETS_OFFSET: usize = 40;
    pub const OFFSETS_LEN: usize = 48;
    pub const TRACE_INDEX_OFFSET: usize = 56;
    pub const TRACE_INDEX_LEN: usize = 64;
    pub const ATTRIBUTE_INDEX_OFFSET: usize = 72;
}

#[test]
fn format_conformance() {
    let records = corpus();
    let bytes = segment::encode(&records).expect("the real encoder produces segment bytes");

    assert_eq!(&bytes[0..8], b"TRAZASEG", "magic");
    // Pin the exported constant to the documented value too: the two once
    // disagreed (the code wrote TRAZAV2 while the docs said TRAZASEG).
    assert_eq!(
        MAGIC, *b"TRAZASEG",
        "the real magic must match the documented format"
    );
    assert_eq!(get_u16(&bytes, offsets::VERSION), VERSION, "format version");
    assert_eq!(get_u16(&bytes, offsets::VERSION), 2, "format version is 2");
    assert_eq!(
        usize::from(get_u16(&bytes, offsets::HEADER_LEN)),
        HEADER_LEN,
        "header length field matches the exported constant"
    );
    assert_eq!(HEADER_LEN, 80, "header is 80 bytes");
    assert_eq!(
        get_u32(&bytes, offsets::RESERVED),
        0,
        "reserved word is zero"
    );
    assert_eq!(
        get_u64(&bytes, offsets::RECORD_COUNT),
        records.len() as u64,
        "record count"
    );

    // The four sections, in the order the format defines them. The reader
    // requires them contiguous from the end of the header to EOF, with no
    // gaps and nothing trailing.
    let sections = [
        (
            get_u64(&bytes, offsets::RECORDS_OFFSET),
            get_u64(&bytes, offsets::RECORDS_LEN),
            "records",
        ),
        (
            get_u64(&bytes, offsets::OFFSETS_OFFSET),
            get_u64(&bytes, offsets::OFFSETS_LEN),
            "record offsets",
        ),
        (
            get_u64(&bytes, offsets::TRACE_INDEX_OFFSET),
            get_u64(&bytes, offsets::TRACE_INDEX_LEN),
            "trace index",
        ),
        (
            get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET),
            // The attribute index runs to EOF; its length is implied by the
            // file size rather than stored, so there is no field to read.
            bytes.len() as u64 - get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET),
            "attribute index",
        ),
    ];
    let mut expected_start = HEADER_LEN as u64;
    for (offset, length, name) in sections {
        assert_eq!(
            offset, expected_start,
            "{name} must start where the previous section ended (contiguous, non-overlapping)"
        );
        let end = offset.checked_add(length).expect("bounded section end");
        assert!(end <= bytes.len() as u64, "{name} must lie within the file");
        expected_start = end;
    }
    assert_eq!(
        expected_start,
        bytes.len() as u64,
        "sections must account for every byte — no trailing slack"
    );

    // The record-offset index is one u64 per record, by construction.
    assert_eq!(
        get_u64(&bytes, offsets::OFFSETS_LEN),
        records.len() as u64 * 8,
        "record-offset index length"
    );

    // Those offsets are relative to the record region and strictly ascending.
    let records_offset = get_u64(&bytes, offsets::RECORDS_OFFSET);
    let records_len = get_u64(&bytes, offsets::RECORDS_LEN);
    let offsets_offset = get_u64(&bytes, offsets::OFFSETS_OFFSET) as usize;
    let mut previous: Option<u64> = None;
    for index in 0..records.len() {
        let relative = get_u64(&bytes, offsets_offset + index * 8);
        assert!(
            relative < records_len,
            "record {index} offset must fall inside the record region"
        );
        if let Some(previous) = previous {
            assert!(
                relative > previous,
                "record offsets must ascend: {relative} after {previous}"
            );
        }
        previous = Some(relative);
    }

    // Hand-decode the first record at the offset the index points to, using
    // the documented record encoding: timestamp u64, trace-id length u32,
    // attribute count u32, payload length u32, reserved u32, then the
    // trace id, length-prefixed attribute pairs, and the payload.
    let first = records_offset as usize + get_u64(&bytes, offsets_offset) as usize;
    assert_eq!(
        get_u64(&bytes, first),
        records[0].timestamp,
        "record timestamp"
    );
    let trace_len = get_u32(&bytes, first + 8) as usize;
    let attribute_count = get_u32(&bytes, first + 12) as usize;
    let payload_len = get_u32(&bytes, first + 16) as usize;
    assert_eq!(get_u32(&bytes, first + 20), 0, "record reserved word");
    let trace_start = first + 24;
    assert_eq!(
        &bytes[trace_start..trace_start + trace_len],
        records[0].trace_id.as_bytes(),
        "trace id"
    );
    assert_eq!(
        attribute_count,
        records[0].attributes.len(),
        "attribute count"
    );
    let mut cursor = trace_start + trace_len;
    for (key, value) in &records[0].attributes {
        let key_len = get_u32(&bytes, cursor) as usize;
        assert_eq!(
            &bytes[cursor + 4..cursor + 4 + key_len],
            key.as_bytes(),
            "attribute key"
        );
        cursor += 4 + key_len;
        let value_len = get_u32(&bytes, cursor) as usize;
        assert_eq!(
            &bytes[cursor + 4..cursor + 4 + value_len],
            value.as_bytes(),
            "attribute value"
        );
        cursor += 4 + value_len;
    }
    assert_eq!(
        &bytes[cursor..cursor + payload_len],
        records[0].payload.as_slice(),
        "payload follows the attributes"
    );

    // Finally: the hand-parsed view and the production parser must agree.
    let parsed = Segment::from_bytes(bytes.clone())
        .expect("the real reader accepts the real encoder's output");
    let header = parsed.header();
    assert_eq!(header.version, get_u16(&bytes, offsets::VERSION));
    assert_eq!(header.record_count, get_u64(&bytes, offsets::RECORD_COUNT));
    assert_eq!(header.records_offset, records_offset);
    assert_eq!(header.records_len, records_len);
    assert_eq!(
        header.offsets_offset,
        get_u64(&bytes, offsets::OFFSETS_OFFSET)
    );
    assert_eq!(
        header.trace_index_offset,
        get_u64(&bytes, offsets::TRACE_INDEX_OFFSET)
    );
    assert_eq!(
        header.attribute_index_offset,
        get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET)
    );

    evidence(
        "format",
        &[
            "magic",
            "version",
            "header_length",
            "reserved_zero",
            "record_count",
            "four_contiguous_sections",
            "sections_in_bounds",
            "no_trailing_bytes",
            "offset_index_length",
            "ascending_record_offsets",
            "record_encoding",
            "hand_parse_matches_production_parse",
        ],
    );
}

#[test]
fn documented_query_semantics() {
    let records = corpus();
    let bytes = segment::encode(&records).expect("encode");
    let parsed = Segment::from_bytes(bytes).expect("open encoded segment");

    assert_eq!(parsed.len(), records.len(), "record count round-trips");

    // Trace lookup returns exactly the records encoded under that trace id.
    let trace_a = parsed.query_trace("trace-a").expect("trace query");
    assert_eq!(trace_a.len(), 2, "trace-a has two records");
    assert!(
        parsed.last_query_used_index(),
        "trace lookup must be served by the index, not a scan"
    );
    let payloads: Vec<&[u8]> = trace_a
        .iter()
        .map(traza::segment::Record::payload)
        .collect();
    assert_eq!(payloads, vec![b"first".as_slice(), b"third".as_slice()]);
    assert!(
        trace_a.iter().all(|record| record.trace_id() == "trace-a"),
        "no foreign trace may leak into the result"
    );

    let trace_b = parsed.query_trace("trace-b").expect("trace query");
    assert_eq!(trace_b.len(), 1);
    assert_eq!(trace_b[0].payload(), b"second");

    assert!(
        parsed
            .query_trace("trace-missing")
            .expect("miss")
            .is_empty(),
        "an absent trace returns no records, not an error"
    );

    // Attribute lookup discriminates on the exact key/value pair.
    let checkout = parsed
        .query_attribute("service", "checkout")
        .expect("attribute query");
    assert_eq!(checkout.len(), 2, "two records carry service=checkout");
    assert!(
        parsed.last_query_used_index(),
        "attribute lookup must be index-served"
    );
    assert!(checkout
        .iter()
        .all(|record| record.attributes().get("service").map(String::as_str) == Some("checkout")));

    let billing = parsed
        .query_attribute("service", "billing")
        .expect("attribute query");
    assert_eq!(billing.len(), 1);
    assert_eq!(billing[0].payload(), b"third");

    assert!(
        parsed
            .query_attribute("service", "absent")
            .expect("miss")
            .is_empty(),
        "an unmatched attribute value returns no records"
    );

    // Time-range filtering is inclusive at both ends.
    let all = parsed
        .query_time_range(1_700_000_000_000_000_000, 1_700_000_000_000_000_200)
        .expect("range query");
    assert_eq!(all.len(), 3, "the inclusive range covers every record");
    let middle = parsed
        .query_time_range(1_700_000_000_000_000_100, 1_700_000_000_000_000_100)
        .expect("range query");
    assert_eq!(middle.len(), 1, "a point range selects one record");
    assert_eq!(middle[0].payload(), b"second");

    // Ordinal access preserves encode order.
    for (index, expected) in records.iter().enumerate() {
        let record = parsed
            .record(index)
            .expect("ordinal access")
            .expect("record present");
        assert_eq!(record.timestamp(), expected.timestamp);
        assert_eq!(record.trace_id(), expected.trace_id);
        assert_eq!(record.payload(), expected.payload.as_slice());
        assert_eq!(record.attributes(), &expected.attributes);
    }

    evidence(
        "query",
        &[
            "trace_index_lookup",
            "trace_miss_is_empty",
            "attribute_index_lookup",
            "attribute_miss_is_empty",
            "inclusive_time_range",
            "stable_ordinal_order",
            "index_served_not_scanned",
        ],
    );
}

#[test]
fn reopen_persistence() {
    let directory = temp_dir("reopen");
    // `.seg` is the only extension the engine writes.
    let path = directory.join("segment-00000000000000000000.seg");
    let records = corpus();
    segment::write(&path, &records).expect("persist segment through the real writer");

    // Reopen through the real reader, not by comparing bytes to themselves:
    // byte equality after a write proves the filesystem works, nothing more.
    let reopened = Segment::open(&path).expect("reopen persisted segment");
    assert_eq!(reopened.header().version, VERSION);
    assert_eq!(reopened.header().record_count, records.len() as u64);
    assert_eq!(reopened.len(), records.len());

    let trace_a = reopened.query_trace("trace-a").expect("trace query");
    assert_eq!(trace_a.len(), 2, "indexes survive the round trip to disk");
    assert_eq!(trace_a[0].payload(), b"first");
    assert_eq!(trace_a[1].payload(), b"third");

    let errors = reopened
        .query_attribute("status", "error")
        .expect("attribute query");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].trace_id(), "trace-b");

    // The bytes on disk are exactly what the encoder produced for this input.
    let on_disk = fs::read(&path).expect("read segment file");
    assert_eq!(
        on_disk,
        segment::encode(&records).expect("encode"),
        "writing is encoding plus an atomic rename — no extra framing"
    );
    assert_eq!(&on_disk[0..8], b"TRAZASEG");

    cleanup(&directory);
    evidence(
        "reopen",
        &[
            "persist_through_writer",
            "reopen_through_reader",
            "header_survives_reopen",
            "trace_index_survives_reopen",
            "attribute_index_survives_reopen",
            "file_bytes_match_encoder",
        ],
    );
}

#[test]
fn byte_residency() {
    let directory = temp_dir("residency");
    let path = directory.join("segment-00000000000000000000.seg");
    let records = corpus();
    segment::write(&path, &records).expect("persist segment");

    // A file-backed segment is the larger-than-RAM path: it holds the header
    // and indexes, never the record payloads.
    let opened = Segment::open(&path).expect("open file-backed segment");
    assert_eq!(
        opened.resident_bytes(),
        0,
        "a file-backed segment must not hold its payload bytes in memory"
    );
    assert_eq!(
        opened.resident_decoded_record_count(),
        0,
        "opening must decode no records"
    );

    // Querying decodes on demand and still retains nothing.
    let found = opened.query_trace("trace-a").expect("trace query");
    assert_eq!(found.len(), 2);
    assert_eq!(
        opened.resident_decoded_record_count(),
        0,
        "querying must not accumulate decoded records in the segment"
    );
    assert_eq!(opened.resident_bytes(), 0);

    // Offsets are how records are addressed; they are recorded per record.
    assert_eq!(opened.record_offsets().len(), records.len());
    let postings = opened.attribute_posting_offsets("service", "checkout");
    assert_eq!(
        postings.len(),
        2,
        "posting list addresses records by offset"
    );
    let record = opened
        .record_at_offset(postings[0])
        .expect("decode at a posting offset");
    assert_eq!(record.payload(), b"first");
    assert_eq!(
        opened
            .timestamp_at(postings[0])
            .expect("timestamp at offset"),
        records[0].timestamp,
        "a timestamp is readable without decoding the whole record"
    );

    // The in-memory path, by contrast, does retain its bytes — the two
    // backings are meant to differ here.
    let resident = Segment::from_bytes(segment::encode(&records).expect("encode")).expect("build");
    assert!(
        resident.resident_bytes() > 0,
        "a memory-backed segment holds its encoding"
    );

    cleanup(&directory);
    evidence(
        "byte_residency",
        &[
            "file_backed_retains_no_payload",
            "open_decodes_no_records",
            "query_retains_no_records",
            "offset_addressed_access",
            "timestamp_without_full_decode",
            "memory_backed_holds_bytes",
        ],
    );
}

#[test]
fn foreign_bytes_are_rejected() {
    // The reader must refuse anything that is not a v2 segment rather than
    // misinterpret it. Legacy v1 JSONL carried no magic at all.
    let legacy = b"{\"timestamp\":1700000000000000000,\"message\":\"legacy\"}\n".to_vec();
    assert_ne!(&legacy[0..8], b"TRAZASEG");
    assert!(
        Segment::from_bytes(legacy).is_err(),
        "a JSONL v1 segment must not parse as v2"
    );

    // A correct magic with the wrong version is refused too.
    let mut wrong_version = segment::encode(&corpus()).expect("encode");
    wrong_version[offsets::VERSION] = 99;
    assert!(
        Segment::from_bytes(wrong_version).is_err(),
        "an unsupported version must be refused"
    );

    // A wrong magic is refused even when everything after it is valid.
    let mut wrong_magic = segment::encode(&corpus()).expect("encode");
    wrong_magic[0] = b'X';
    assert!(
        Segment::from_bytes(wrong_magic).is_err(),
        "a foreign magic must be refused"
    );

    // Sections must be contiguous: stretching the record region leaves the
    // next section starting somewhere other than where records end.
    let full = segment::encode(&corpus()).expect("encode");
    let mut gapped = full.clone();
    let records_len = get_u64(&gapped, offsets::RECORDS_LEN);
    gapped[offsets::RECORDS_LEN..offsets::RECORDS_LEN + 8]
        .copy_from_slice(&(records_len + 1).to_le_bytes());
    assert!(
        Segment::from_bytes(gapped).is_err(),
        "non-contiguous sections must be refused"
    );

    // Truncating the file below the attribute index offset leaves the last
    // section starting past EOF.
    let attribute_offset = get_u64(&full, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
    let truncated = full[..attribute_offset - 1].to_vec();
    assert!(
        Segment::from_bytes(truncated).is_err(),
        "truncation past a section start must be refused"
    );

    evidence(
        "rejection",
        &[
            "legacy_jsonl_not_parsed_as_v2",
            "unsupported_version_refused",
            "foreign_magic_refused",
            "non_contiguous_sections_refused",
            "truncation_refused",
        ],
    );
}
