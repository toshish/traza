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
    /// The inclusive record timestamp range.
    pub const MIN_TIMESTAMP: usize = 80;
    pub const MAX_TIMESTAMP: usize = 88;
    /// The content index, which is what bounds the attribute index.
    pub const CONTENT_INDEX_OFFSET: usize = 96;
}

/// Where the attribute index ends: the content index's offset.
fn attribute_index_end(bytes: &[u8]) -> usize {
    get_u64(bytes, offsets::CONTENT_INDEX_OFFSET) as usize
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
    assert_eq!(
        get_u16(&bytes, offsets::VERSION),
        VERSION,
        "the encoder writes the one supported version"
    );
    assert_eq!(
        VERSION, 6,
        "one readable format, numbered after every version that shipped before it"
    );
    assert_eq!(
        usize::from(get_u16(&bytes, offsets::HEADER_LEN)),
        HEADER_LEN,
        "header length field matches the exported constant"
    );
    assert_eq!(HEADER_LEN, 104, "header is 104 bytes");

    // The timestamp range, hand-parsed at its documented offsets. This is
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
            // Neither the attribute nor the content index stores its own
            // length. The attribute index is bounded by where the content
            // index starts, and the content index runs to EOF.
            attribute_index_end(&bytes) as u64 - get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET),
            "attribute index",
        ),
        (
            get_u64(&bytes, offsets::CONTENT_INDEX_OFFSET),
            bytes.len() as u64 - get_u64(&bytes, offsets::CONTENT_INDEX_OFFSET),
            "content index",
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
        attribute_index_end(&bytes),
        "the attribute section must account for every byte up to the content index"
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
    let section = &bytes[attribute_start..attribute_index_end(&bytes)];
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
    let attribute_end = attribute_index_end(bytes);
    assert_eq!(
        cursor, attribute_end,
        "consumed the whole attribute section"
    );

    // Inflating the posting lists moves the content index, so its offset in
    // the header has to move with it or the reader rejects the file for
    // non-contiguous sections.
    let mut out = bytes[..attribute_start].to_vec();
    let content_offset = attribute_start + section.len();
    out[offsets::CONTENT_INDEX_OFFSET..offsets::CONTENT_INDEX_OFFSET + 8]
        .copy_from_slice(&(content_offset as u64).to_le_bytes());
    out.extend_from_slice(&section);
    out.extend_from_slice(&bytes[attribute_end..]);
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
fn the_timestamp_range_is_always_readable_and_can_rule_a_segment_out() {
    // Every segment carries a range, so pruning never has to fall back to
    // "unknown, scan it". That fallback used to exist for segments predating
    // the field, and it meant a time-bounded query opened them in full.
    let records = corpus();
    let bytes = segment::encode(&records).expect("encode");
    let segment = Segment::from_bytes(bytes).expect("opens");

    let (min, max) = segment.timestamp_range();
    assert_eq!(min, records.iter().map(|r| r.timestamp).min().expect("min"));
    assert_eq!(max, records.iter().map(|r| r.timestamp).max().expect("max"));

    assert!(segment.may_contain_timestamps(Some(min), Some(max)));
    assert!(
        !segment.may_contain_timestamps(Some(max + 1), None),
        "a segment strictly before the window is skipped without being read"
    );
    assert!(
        !segment.may_contain_timestamps(None, Some(min - 1)),
        "and likewise one strictly after it"
    );

    // An empty segment encodes an empty range, which is a real answer rather
    // than an unknown one: it overlaps nothing and is always skippable.
    let empty = Segment::from_bytes(segment::encode(&[]).expect("encode empty")).expect("opens");
    let (empty_min, empty_max) = empty.timestamp_range();
    assert!(empty_min > empty_max, "empty range, not unknown");
    assert!(!empty.may_contain_timestamps(None, None));

    evidence(
        "timestamp_range",
        &[
            "range_always_present",
            "segment_before_window_skipped",
            "segment_after_window_skipped",
            "empty_segment_is_empty_not_unknown",
        ],
    );
}

#[test]
fn a_superseded_format_is_refused_rather_than_misread() {
    // The version word earns its two bytes here. Every superseded format had
    // this magic and a plausible header, so without the check their fields
    // would be read at THIS format's offsets — producing section bounds that
    // pass validation while addressing the wrong bytes. Refusing to open is
    // the only outcome that surfaces as a problem rather than as data.
    //
    // 1 through 5 are covered because all five were written by tagged releases:
    // 1 was JSONL, 2 shipped in 0.16/0.17, 3 in 0.18/0.19, 5 immediately before
    // this. Those identifiers stay spent — reusing one for a different layout
    // would make a header declaring it ambiguous between two incompatible
    // files, which is the exact failure the field exists to prevent.
    let records = corpus();
    for stale in [1_u16, 2, 3, 4, 5] {
        let mut bytes = segment::encode(&records).expect("encode");
        bytes[offsets::VERSION..offsets::VERSION + 2].copy_from_slice(&stale.to_le_bytes());
        assert!(
            Segment::from_bytes(bytes).is_err(),
            "version {stale} is not this format and must be refused"
        );
    }

    // A FUTURE version is refused on the same reasoning, which is what keeps
    // this constant worth carrying after release.
    let mut future = segment::encode(&records).expect("encode");
    future[offsets::VERSION..offsets::VERSION + 2].copy_from_slice(&(VERSION + 1).to_le_bytes());
    assert!(Segment::from_bytes(future).is_err(), "a future format too");

    evidence(
        "single_version",
        &["superseded_versions_refused", "future_version_refused"],
    );
}

#[test]
fn the_content_index_holds_filters_and_never_the_text() {
    // The content index exists so that text can be SEARCHED without being
    // STORED. That is a property of the bytes, and it is the one a reader
    // test cannot check: a reader that kept a copy of every indexed word
    // would answer every query correctly and cost exactly what the index was
    // built to avoid.
    let records = vec![
        RecordInput::new(
            1_700_000_000_000_000_000,
            "trace-a",
            attributes(&[("service", "checkout")]),
            b"first".to_vec(),
        )
        .with_content(vec![
            "please issue a refund for the antidisestablishment order".to_owned(),
        ]),
        RecordInput::new(
            1_700_000_000_000_000_100,
            "trace-b",
            attributes(&[("service", "billing")]),
            b"second".to_vec(),
        )
        .with_content(vec!["the quarterly summary is ready".to_owned()]),
    ];
    let bytes = segment::encode(&records).expect("encode");

    let start = get_u64(&bytes, offsets::CONTENT_INDEX_OFFSET) as usize;
    let section = &bytes[start..];

    // Prologue, hand-decoded at its documented layout.
    assert_eq!(get_u32(section, 0), 0, "reserved word is zero");
    let block_records = get_u32(section, 4);
    let block_count = get_u32(section, 8);
    let hash_count = get_u32(section, 12);
    let summary_bits = get_u64(section, 16) as usize;
    let block_bits = get_u64(section, 24) as usize;
    assert_eq!(block_records, 128, "documented block size");
    assert_eq!(block_count, 1, "two records fit in one block");
    assert_eq!(hash_count, 4, "documented hash count");
    assert!(summary_bits.is_power_of_two() && summary_bits >= 8);
    assert!(block_bits.is_power_of_two() && block_bits >= 8);

    // The section is exactly the prologue, the summary filter, and one
    // bit-sliced row per bit position.
    let row_bytes = (block_count as usize).div_ceil(8);
    assert_eq!(
        section.len(),
        32 + summary_bits / 8 + block_bits * row_bytes,
        "the content section must account for every byte"
    );

    // No indexed WORD may appear anywhere in it. These words are distinctive
    // enough that a byte search is a fair test, and this assertion is what
    // would fail if a future change stored a token list beside the filter.
    for word in [
        "antidisestablishment",
        "refund",
        "quarterly",
        "summary",
        "please",
    ] {
        assert!(
            !contains(section, word.as_bytes()),
            "the content index must not store the word {word:?}"
        );
    }

    // And it still answers. Reading through the real segment: a word that is
    // present must be admitted, and one that is absent must be rejected.
    let opened = Segment::from_bytes(bytes).expect("open");
    assert!(opened.has_content_index());
    let present = traza::content::Query::new("antidisestablishment");
    assert!(opened.may_contain_content(&present));
    assert_eq!(
        opened
            .content_candidate_offsets(&present)
            .expect("probe")
            .expect("an indexable query is narrowed")
            .len(),
        2,
        "both records share the one block, so both are candidates"
    );

    let absent = traza::content::Query::new("zygomorphic");
    assert!(
        !opened.may_contain_content(&absent),
        "a word no record holds must be ruled out by the resident summary"
    );

    evidence(
        "content_index",
        &[
            "prologue_layout",
            "section_accounts_for_every_byte",
            "no_indexed_word_is_stored",
            "present_word_is_admitted",
            "absent_word_is_ruled_out",
        ],
    );
}

#[test]
fn a_segment_without_a_content_index_is_never_skipped_by_a_content_query() {
    // Same discipline as the v2 timestamp range: absent must read as
    // UNKNOWN, never as empty. A segment written before v5, or one carrying
    // no indexable text, must be scanned. Reading absence as "holds nothing"
    // would make content search silently return no rows.
    let records = corpus(); // built with RecordInput::new -- no content
    let bytes = segment::encode(&records).expect("encode");
    let opened = Segment::from_bytes(bytes).expect("open");
    assert!(
        !opened.has_content_index(),
        "records with no content text produce no content index"
    );
    let query = traza::content::Query::new("anything");
    assert!(
        opened.may_contain_content(&query),
        "an absent index must never rule a segment out"
    );
    assert!(
        opened
            .content_candidate_offsets(&query)
            .expect("probe")
            .is_none(),
        "and it must narrow nothing, so every record stays a candidate"
    );

    // A segment encoded with indexing switched off reads the same way: the
    // section is present and declares zero blocks, so "indexed nothing" is
    // stated rather than inferred from an absent section.
    let unindexed = segment::encode_with(&records, false).expect("encode");
    let opened = Segment::from_bytes(unindexed).expect("opens");
    assert!(!opened.has_content_index());
    assert!(opened.may_contain_content(&query));

    evidence(
        "content_index_absent",
        &[
            "absent_index_never_prunes",
            "unindexed_segment_never_prunes",
        ],
    );
}
