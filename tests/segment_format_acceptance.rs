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

use traza::segment::{self, RecordInput, Segment, HEADER_LEN, HEADER_LEN_V2, MAGIC, VERSION};

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
    /// v3 additions: the inclusive record timestamp range.
    pub const MIN_TIMESTAMP: usize = 80;
    pub const MAX_TIMESTAMP: usize = 88;
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
    assert_eq!(get_u16(&bytes, offsets::VERSION), 4, "format version is 4");
    assert_eq!(
        usize::from(get_u16(&bytes, offsets::HEADER_LEN)),
        HEADER_LEN,
        "header length field matches the exported constant"
    );
    assert_eq!(HEADER_LEN, 96, "header is 96 bytes");
    assert_eq!(HEADER_LEN_V2, 80, "the v2 header this reader still accepts");

    // The v3 timestamp range, hand-parsed at its documented offsets. This is
    // the field a query uses to skip a segment without opening it, so a
    // wrong value here silently drops results rather than corrupting a read.
    let expected_min = records.iter().map(|r| r.timestamp).min().expect("records");
    let expected_max = records.iter().map(|r| r.timestamp).max().expect("records");
    assert_eq!(
        get_u64(&bytes, offsets::MIN_TIMESTAMP),
        expected_min,
        "minimum record timestamp"
    );
    assert_eq!(
        get_u64(&bytes, offsets::MAX_TIMESTAMP),
        expected_max,
        "maximum record timestamp"
    );
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

    // The v4 attribute section, hand-decoded at its documented layout: a key
    // dictionary (u32 count, then length-prefixed names), then entries of
    // (u32 key id, 16-byte value digest, u32 posting count, u64 postings).
    //
    // The point of the hand-decode is that the VALUE TEXT IS NOT THERE. That
    // is the entire memory fix, and it is a property of the bytes, not of the
    // reader — a reader that quietly kept a copy would still pass every query
    // test in this file.
    let attribute_start = get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
    let mut cursor = attribute_start;
    let key_count = get_u32(&bytes, cursor) as usize;
    cursor += 4;
    let mut dictionary = Vec::new();
    for _ in 0..key_count {
        let key_len = get_u32(&bytes, cursor) as usize;
        cursor += 4;
        dictionary.push(std::str::from_utf8(&bytes[cursor..cursor + key_len]).expect("utf-8 key"));
        cursor += key_len;
    }
    let expected_keys: std::collections::BTreeSet<&str> = records
        .iter()
        .flat_map(|record| record.attributes.keys().map(String::as_str))
        .collect();
    assert_eq!(
        dictionary,
        expected_keys.iter().copied().collect::<Vec<&str>>(),
        "the key dictionary holds every distinct attribute key, sorted"
    );

    let entry_count = get_u32(&bytes, cursor) as usize;
    cursor += 4;
    let mut total_postings = 0usize;
    for _ in 0..entry_count {
        let key_id = get_u32(&bytes, cursor) as usize;
        assert!(key_id < key_count, "entry names a key in the dictionary");
        cursor += 4;
        // The digest occupies exactly 16 bytes; no length prefix, because a
        // digest has a fixed width where a value did not.
        cursor += 16;
        let posting_count = get_u32(&bytes, cursor) as usize;
        cursor += 4;
        total_postings += posting_count;
        cursor += posting_count * 8;
    }
    assert_eq!(
        cursor,
        bytes.len(),
        "the attribute section must account for every remaining byte"
    );
    assert_eq!(
        total_postings,
        records
            .iter()
            .map(|record| record.attributes.len())
            .sum::<usize>(),
        "every (record, attribute) pair is posted exactly once"
    );

    // No attribute VALUE may appear anywhere in the attribute section. The
    // corpus values are distinctive enough that a substring search is a fair
    // test, and this is the assertion that would fail if a future change
    // reintroduced value text into the index.
    let section = &bytes[attribute_start..];
    for record in &records {
        for value in record.attributes.values() {
            assert!(
                !contains(section, value.as_bytes()),
                "attribute value {value:?} must not be stored in the v4 index"
            );
        }
    }

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
            "attribute_key_dictionary",
            "attribute_entries_are_digest_keyed",
            "no_attribute_value_text_in_the_index",
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
    let postings = opened
        .attribute_candidate_offsets("service", "checkout")
        .to_vec();
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

/// Rewrites v3 bytes into a genuine v2 segment: version 2, an 80-byte header
/// with no timestamp range, and every section offset shifted down by the 16
/// bytes the range occupied.
///
/// Built by transformation rather than kept as a fixture because a checked-in
/// binary blob rots silently — nothing would tell us it had stopped
/// resembling what 0.16 actually wrote.
/// Builds a genuinely OLD segment: a v2 or v3 header over the pre-v4
/// attribute section, which stored attribute value TEXT rather than a digest.
///
/// An earlier version of this helper only rewrote the header and kept the
/// current section bytes, which tested nothing about reading old files — the
/// result was a version number the engine had never actually written. Real
/// legacy bytes are what exercise the upgrade-on-open path, and that path is
/// the reason `MIN_READABLE_VERSION` is still 2.
fn encode_legacy(records: &[RecordInput], version: u16) -> Vec<u8> {
    assert!((2..=3).contains(&version), "legacy means v2 or v3");
    let current = segment::encode(records).expect("encode");
    let attribute_start = get_u64(&current, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
    let offsets_offset = get_u64(&current, offsets::OFFSETS_OFFSET) as usize;

    // Rebuild the postings against the real record offsets, then write them
    // in the pre-v4 encoding: key text, value text, then the offsets.
    let mut index: BTreeMap<(&str, &str), Vec<u64>> = BTreeMap::new();
    for (ordinal, record) in records.iter().enumerate() {
        let offset = get_u64(&current, offsets_offset + ordinal * 8);
        for (key, value) in &record.attributes {
            index
                .entry((key.as_str(), value.as_str()))
                .or_default()
                .push(offset);
        }
    }
    let mut section = Vec::new();
    section.extend_from_slice(&(index.len() as u32).to_le_bytes());
    for ((key, value), postings) in &index {
        section.extend_from_slice(&(key.len() as u32).to_le_bytes());
        section.extend_from_slice(key.as_bytes());
        section.extend_from_slice(&(value.len() as u32).to_le_bytes());
        section.extend_from_slice(value.as_bytes());
        section.extend_from_slice(&(postings.len() as u32).to_le_bytes());
        for offset in postings {
            section.extend_from_slice(&offset.to_le_bytes());
        }
    }

    // Records, record offsets, and the trace index are unchanged from v3, so
    // reuse the real encoder's bytes for them and swap only the tail.
    let mut out = current[..attribute_start].to_vec();
    out.extend_from_slice(&section);
    out[8..10].copy_from_slice(&version.to_le_bytes());
    if version == 2 {
        // v2 has no timestamp range, so its header is 16 bytes shorter and
        // every section slides down by that much.
        const SHIFT: u64 = (HEADER_LEN - HEADER_LEN_V2) as u64;
        let mut header = out[..HEADER_LEN_V2].to_vec();
        header[10..12].copy_from_slice(&(HEADER_LEN_V2 as u16).to_le_bytes());
        for offset_field in [24_usize, 40, 56, 72] {
            let value = get_u64(&out, offset_field) - SHIFT;
            header[offset_field..offset_field + 8].copy_from_slice(&value.to_le_bytes());
        }
        let mut shifted = header;
        shifted.extend_from_slice(&out[HEADER_LEN..]);
        return shifted;
    }
    out
}

/// Whether `haystack` contains `needle` as a contiguous byte run.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Rewrites the v4 attribute section so every entry posts every record — the
/// shape a digest collision produces, made deterministic.
///
/// A 128-bit digest will not collide naturally inside a test, or inside a
/// human lifetime of running Traza. That is exactly why the safety argument
/// cannot be left to chance: no test that merely queries a normal segment can
/// tell a verifying reader from a trusting one. Forging the collision is the
/// only way to make the difference observable.
fn forge_digest_collisions(bytes: &[u8], record_count: usize) -> Vec<u8> {
    let attribute_start = get_u64(bytes, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
    let offsets_offset = get_u64(bytes, offsets::OFFSETS_OFFSET) as usize;
    let all: Vec<u64> = (0..record_count)
        .map(|ordinal| get_u64(bytes, offsets_offset + ordinal * 8))
        .collect();

    let mut cursor = attribute_start;
    let key_count = get_u32(bytes, cursor) as usize;
    cursor += 4;
    let dictionary_start = cursor;
    for _ in 0..key_count {
        cursor += 4 + get_u32(bytes, cursor) as usize;
    }
    let dictionary = &bytes[dictionary_start..cursor];
    let entry_count = get_u32(bytes, cursor) as usize;
    cursor += 4;

    let mut section = Vec::new();
    section.extend_from_slice(&(key_count as u32).to_le_bytes());
    section.extend_from_slice(dictionary);
    section.extend_from_slice(&(entry_count as u32).to_le_bytes());
    for _ in 0..entry_count {
        section.extend_from_slice(&bytes[cursor..cursor + 4]); // key id
        cursor += 4;
        section.extend_from_slice(&bytes[cursor..cursor + 16]); // digest
        cursor += 16;
        let posting_count = get_u32(bytes, cursor) as usize;
        cursor += 4 + posting_count * 8;
        section.extend_from_slice(&(all.len() as u32).to_le_bytes());
        for offset in &all {
            section.extend_from_slice(&offset.to_le_bytes());
        }
    }
    assert_eq!(cursor, bytes.len(), "consumed the whole attribute section");

    let mut out = bytes[..attribute_start].to_vec();
    out.extend_from_slice(&section);
    out
}

#[test]
fn a_digest_collision_cannot_produce_a_wrong_row() {
    // The v4 index answers with candidates, so correctness rests entirely on
    // the reader checking each candidate against the record it points at.
    // Here every posting list names every record; a reader that trusted the
    // index would return all three rows for every query.
    let records = corpus();
    let honest = segment::encode(&records).expect("encode");
    let forged = forge_digest_collisions(&honest, records.len());

    let segment = Segment::from_bytes(forged).expect("the forged segment still parses");

    let checkout = segment
        .query_attribute("service", "checkout")
        .expect("attribute query");
    assert_eq!(
        checkout.len(),
        2,
        "verification must discard the candidates that do not hold the value"
    );
    assert!(checkout
        .iter()
        .all(|record| record.attributes()["service"] == "checkout"));

    let billing = segment
        .query_attribute("service", "billing")
        .expect("attribute query");
    assert_eq!(billing.len(), 1);
    assert_eq!(billing[0].payload(), b"third");

    // A value no record holds must come back empty even though the index
    // offers a full posting list for its key.
    assert_eq!(
        segment
            .query_attribute("status", "cancelled")
            .expect("miss")
            .len(),
        0,
        "a candidate list must never invent a match"
    );

    // The unverified surface is honest about what it returns: three
    // candidates, which is what makes the check above load-bearing.
    assert_eq!(
        segment
            .attribute_candidate_offsets("service", "checkout")
            .len(),
        3,
        "the probe itself is a superset, by construction"
    );

    evidence(
        "collision_safety",
        &[
            "forged_collision_parses",
            "query_attribute_verifies_candidates",
            "absent_value_returns_empty",
            "probe_is_a_superset",
        ],
    );
}

#[test]
fn an_old_segments_value_text_is_dropped_when_it_is_opened() {
    // v2 and v3 stored attribute values as text. Opening one must not carry
    // that text into memory — otherwise the memory fix applies only to data
    // written after the upgrade, and an operator's existing store keeps
    // paying the old cost until every segment happens to be rewritten.
    let records = corpus();
    for version in [2_u16, 3] {
        let bytes = encode_legacy(&records, version);
        assert_eq!(
            get_u16(&bytes, offsets::VERSION),
            version,
            "fixture really is v{version}"
        );
        // Sanity: the fixture genuinely contains the old value text, so the
        // assertion below is testing the reader rather than an empty section.
        let attribute_start = get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
        assert!(
            contains(&bytes[attribute_start..], b"checkout"),
            "the v{version} fixture must actually store value text"
        );

        let segment = Segment::from_bytes(bytes).expect("an old segment still opens");

        // Queries still work, through the digest index built at open time.
        let found = segment
            .query_attribute("service", "checkout")
            .expect("attribute query");
        assert_eq!(found.len(), 2, "v{version} attribute lookup still resolves");
        assert!(
            found
                .iter()
                .all(|record| record.attributes()["service"] == "checkout"),
            "no foreign value may leak through the upgraded index"
        );
        assert_eq!(
            segment
                .query_attribute("service", "absent")
                .expect("miss")
                .len(),
            0
        );

        // And the resident index is the small one. Three records with two
        // attributes each cannot exceed a few hundred bytes of postings and
        // dictionary; the old form would additionally hold every value.
        assert_eq!(
            segment.attribute_index_len(),
            4,
            "four distinct (key, value) pairs in the corpus"
        );
    }

    evidence(
        "legacy_upgrade",
        &[
            "v2_opens",
            "v3_opens",
            "value_text_present_in_fixture",
            "attribute_query_resolves_after_upgrade",
            "upgraded_index_cardinality",
        ],
    );
}

#[test]
fn a_v2_segment_still_reads_and_is_never_skipped_by_a_time_filter() {
    // A v2 segment carries no timestamp range. "Unknown" has to mean "cannot
    // rule this out" — if it were read as an empty range instead, every v2
    // segment in a store would vanish from every time-filtered query, which
    // is data loss that looks like an empty result.
    let records = corpus();
    let v3 = segment::encode(&records).expect("encode");
    let v2 = encode_legacy(&records, 2);

    assert_eq!(get_u16(&v2, offsets::VERSION), 2, "encoded as v2");
    assert_eq!(
        usize::from(get_u16(&v2, offsets::HEADER_LEN)),
        HEADER_LEN_V2
    );

    let segment = Segment::from_bytes(v2).expect("a v2 segment still opens");
    assert_eq!(
        segment.header().record_count,
        records.len() as u64,
        "every record is still readable"
    );
    assert_eq!(
        segment.timestamp_range(),
        None,
        "v2 carries no range, so the range is unknown rather than empty"
    );

    // The load-bearing assertion: unknown range means the segment must be
    // considered for every window, including ones it may not actually
    // intersect.
    assert!(
        segment.may_contain_timestamps(Some(0), Some(1)),
        "a v2 segment must never be skipped"
    );
    assert!(segment.may_contain_timestamps(Some(u64::MAX - 1), None));
    assert!(segment.may_contain_timestamps(None, Some(0)));

    // And a v3 segment over the same records DOES carry a usable range.
    let v3_segment = Segment::from_bytes(v3).expect("v3 opens");
    let (min, max) = v3_segment.timestamp_range().expect("v3 has a range");
    assert!(min <= max);
    assert!(
        !v3_segment.may_contain_timestamps(Some(max + 1), None),
        "v3 can be ruled out, which is the whole point of the field"
    );
}
