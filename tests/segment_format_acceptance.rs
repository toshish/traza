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
//! For format v7 the hand-parse covers the compression machinery too: the
//! block directory is walked entry by entry, block CRCs are recomputed by a
//! test-local implementation of the documented polynomial, and the logical
//! record region is reconstructed by inflating each block — then compared
//! byte for byte against a raw-codec encoding of the same records, which is
//! what pins "codec choice changes storage, never meaning".
//!
//! Each test emits one JSON evidence record so verification can distinguish the
//! behavioral categories without relying on test names alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use traza::hash::{hash_attribute, Hash128};
use traza::segment::{
    self, Codec, RecordInput, Segment, COMPRESSION_BLOCK_BYTES, DIRECTORY_ENTRY_LEN, HEADER_LEN,
    MAGIC, VERSION,
};

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

/// CRC-32 over the IEEE/gzip polynomial, implemented HERE so the directory's
/// checksum field is checked against the documented algorithm rather than
/// against whatever the crate computes.
fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn attributes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The corpus every layout test in this file encodes. Two traces, overlapping
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
    /// v6's reserved word, spent on the codec id in v7.
    pub const CODEC: usize = 12;
    pub const RECORD_COUNT: usize = 16;
    pub const RECORDS_OFFSET: usize = 24;
    /// The records region AS STORED (compressed bytes).
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
    /// The three u64s v7 appends.
    pub const DIRECTORY_OFFSET: usize = 104;
    pub const DIRECTORY_LEN: usize = 112;
    pub const RECORDS_LOGICAL_LEN: usize = 120;
}

/// Where the attribute index ends: the content index's offset.
fn attribute_index_end(bytes: &[u8]) -> usize {
    get_u64(bytes, offsets::CONTENT_INDEX_OFFSET) as usize
}

/// One hand-parsed block-directory entry.
#[derive(Clone, Copy, Debug)]
struct DirEntry {
    logical_start: u64,
    stored_offset: u64,
    stored_len: u32,
    raw: bool,
    crc32: u32,
    min_timestamp: u64,
}

/// Hand-parses the block directory at its documented 32-byte-entry layout.
fn parse_directory(bytes: &[u8]) -> Vec<DirEntry> {
    let start = get_u64(bytes, offsets::DIRECTORY_OFFSET) as usize;
    let length = get_u64(bytes, offsets::DIRECTORY_LEN) as usize;
    assert_eq!(
        length % DIRECTORY_ENTRY_LEN,
        0,
        "the directory is whole 32-byte entries"
    );
    (0..length / DIRECTORY_ENTRY_LEN)
        .map(|index| {
            let base = start + index * DIRECTORY_ENTRY_LEN;
            let word = get_u32(bytes, base + 16);
            DirEntry {
                logical_start: get_u64(bytes, base),
                stored_offset: get_u64(bytes, base + 8),
                stored_len: word & 0x7fff_ffff,
                raw: word & (1 << 31) != 0,
                crc32: get_u32(bytes, base + 20),
                min_timestamp: get_u64(bytes, base + 24),
            }
        })
        .collect()
}

/// Reconstructs the LOGICAL record region by walking the directory and
/// inflating each block: raw blocks are copied, compressed ones inflated to
/// their logical extent. Every block's CRC is recomputed on the way.
fn inflate_records_region(bytes: &[u8]) -> Vec<u8> {
    let records_offset = get_u64(bytes, offsets::RECORDS_OFFSET) as usize;
    let logical_len = get_u64(bytes, offsets::RECORDS_LOGICAL_LEN);
    let codec = get_u32(bytes, offsets::CODEC);
    let directory = parse_directory(bytes);
    let mut region: Vec<u8> = Vec::new();
    for (index, entry) in directory.iter().enumerate() {
        let stored =
            &bytes[records_offset + entry.stored_offset as usize..][..entry.stored_len as usize];
        assert_eq!(
            crc32_ieee(stored),
            entry.crc32,
            "block {index} carries the CRC-32 of its stored bytes"
        );
        let extent = directory
            .get(index + 1)
            .map_or(logical_len, |next| next.logical_start)
            - entry.logical_start;
        if entry.raw {
            assert_eq!(
                stored.len() as u64,
                extent,
                "a raw block's stored bytes are its logical bytes"
            );
            region.extend_from_slice(stored);
        } else {
            assert_eq!(codec, 1, "only the lz4 codec stores non-raw blocks here");
            let inflated =
                lz4_flex::block::decompress(stored, extent as usize).expect("block inflates");
            assert_eq!(
                inflated.len() as u64,
                extent,
                "block inflates to its extent"
            );
            region.extend_from_slice(&inflated);
        }
    }
    assert_eq!(
        region.len() as u64,
        logical_len,
        "the directory accounts for the whole logical region"
    );
    region
}

/// The key dictionary as the attribute section stores it: distinct keys,
/// sorted, length-prefixed.
fn parse_dictionary(bytes: &[u8]) -> Vec<String> {
    let start = get_u64(bytes, offsets::ATTRIBUTE_INDEX_OFFSET) as usize;
    let mut cursor = start;
    let key_count = get_u32(bytes, cursor) as usize;
    cursor += 4;
    let mut dictionary = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        let key_len = get_u32(bytes, cursor) as usize;
        cursor += 4;
        dictionary.push(
            std::str::from_utf8(&bytes[cursor..cursor + key_len])
                .expect("utf-8 key")
                .to_owned(),
        );
        cursor += key_len;
    }
    dictionary
}

/// The `(key id, digest)` pairs a record is expected to store, derived from
/// the corpus exactly as the format defines them: ids index the sorted set of
/// distinct keys, digests are `hash_attribute(key, value)`.
fn expected_pairs(all: &[RecordInput], record: &RecordInput) -> Vec<(u32, Hash128)> {
    let keys: Vec<&String> = all
        .iter()
        .flat_map(|record| record.attributes.keys())
        .collect::<BTreeSet<&String>>()
        .into_iter()
        .collect();
    record
        .attributes
        .iter()
        .map(|(key, value)| {
            let id = keys
                .iter()
                .position(|held| *held == key)
                .expect("key in dictionary") as u32;
            (id, hash_attribute(key, value))
        })
        .collect()
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
        VERSION, 7,
        "one readable format, numbered after every version that shipped before it"
    );
    assert_eq!(
        usize::from(get_u16(&bytes, offsets::HEADER_LEN)),
        HEADER_LEN,
        "header length field matches the exported constant"
    );
    assert_eq!(HEADER_LEN, 128, "header is 128 bytes");
    assert_eq!(
        get_u32(&bytes, offsets::CODEC),
        Codec::Lz4.id(),
        "the default encoder writes the lz4 codec id in v6's reserved word"
    );

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
        get_u64(&bytes, offsets::RECORD_COUNT),
        records.len() as u64,
        "record count"
    );

    // The six sections, in the order the format defines them. The reader
    // requires them contiguous from the end of the header to EOF, with no
    // gaps and nothing trailing.
    let sections = [
        (
            get_u64(&bytes, offsets::RECORDS_OFFSET),
            get_u64(&bytes, offsets::RECORDS_LEN),
            "stored records",
        ),
        (
            get_u64(&bytes, offsets::DIRECTORY_OFFSET),
            get_u64(&bytes, offsets::DIRECTORY_LEN),
            "block directory",
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

    // Those offsets are LOGICAL — relative to the uncompressed record region
    // — and strictly ascending.
    let logical_len = get_u64(&bytes, offsets::RECORDS_LOGICAL_LEN);
    let offsets_offset = get_u64(&bytes, offsets::OFFSETS_OFFSET) as usize;
    let mut previous: Option<u64> = None;
    for index in 0..records.len() {
        let relative = get_u64(&bytes, offsets_offset + index * 8);
        assert!(
            relative < logical_len,
            "record {index} offset must fall inside the LOGICAL record region"
        );
        if let Some(previous) = previous {
            assert!(
                relative > previous,
                "record offsets must ascend: {relative} after {previous}"
            );
        }
        previous = Some(relative);
    }

    // Directory arithmetic, hand-checked in full: logical starts from zero
    // and strictly ascending, stored blocks contiguous from zero, masked
    // lengths summing to the stored region, every block starting at a record
    // boundary, min timestamps equal to each block's first record.
    let directory = parse_directory(&bytes);
    assert!(!directory.is_empty(), "a non-empty segment has blocks");
    let record_offsets: Vec<u64> = (0..records.len())
        .map(|index| get_u64(&bytes, offsets_offset + index * 8))
        .collect();
    let mut stored_total = 0u64;
    for (index, entry) in directory.iter().enumerate() {
        if index == 0 {
            assert_eq!(entry.logical_start, 0, "the first block starts at zero");
        } else {
            assert!(
                entry.logical_start > directory[index - 1].logical_start,
                "logical starts are strictly increasing"
            );
            assert!(
                entry.min_timestamp >= directory[index - 1].min_timestamp,
                "min timestamps are sorted, which is what the window probe searches"
            );
        }
        assert_eq!(
            entry.stored_offset, stored_total,
            "stored blocks are contiguous from zero"
        );
        stored_total += u64::from(entry.stored_len);
        let boundary = record_offsets
            .iter()
            .position(|offset| *offset == entry.logical_start)
            .expect("every block starts at a record boundary");
        assert_eq!(
            entry.min_timestamp,
            get_u64(
                &inflate_records_region(&bytes),
                record_offsets[boundary] as usize
            ),
            "a block's min timestamp is its first record's timestamp"
        );
    }
    assert_eq!(
        stored_total,
        get_u64(&bytes, offsets::RECORDS_LEN),
        "the masked stored lengths sum to the stored records length"
    );

    // Codec choice changes storage, never meaning: the logical region
    // reconstructed from the lz4 file equals a raw-codec encoding's records
    // region byte for byte, and so does every index section.
    let logical = inflate_records_region(&bytes);
    let raw_file =
        segment::encode_with_codec(&records, true, Codec::Raw).expect("raw-codec encode");
    assert_eq!(get_u32(&raw_file, offsets::CODEC), Codec::Raw.id());
    let raw_records_offset = get_u64(&raw_file, offsets::RECORDS_OFFSET) as usize;
    let raw_records_len = get_u64(&raw_file, offsets::RECORDS_LEN) as usize;
    assert_eq!(
        raw_records_len as u64,
        get_u64(&raw_file, offsets::RECORDS_LOGICAL_LEN),
        "under codec 0 the stored and logical lengths coincide"
    );
    assert_eq!(
        logical,
        raw_file[raw_records_offset..raw_records_offset + raw_records_len].to_vec(),
        "the logical record bytes are codec-independent"
    );
    let index_sections =
        |file: &[u8]| file[get_u64(file, offsets::OFFSETS_OFFSET) as usize..].to_vec();
    assert_eq!(
        index_sections(&bytes),
        index_sections(&raw_file),
        "offset, trace, attribute, and content sections are codec-independent"
    );

    // Hand-decode the first record at the offset the index points to, using
    // the documented record encoding: timestamp u64, trace-id length u32,
    // attribute count u32, payload length u32, reserved u32, then the
    // trace id, then FIXED 20-byte (key id, digest) pairs, then the payload.
    let first = record_offsets[0] as usize;
    assert_eq!(
        get_u64(&logical, first),
        records[0].timestamp,
        "record timestamp"
    );
    let trace_len = get_u32(&logical, first + 8) as usize;
    let attribute_count = get_u32(&logical, first + 12) as usize;
    let payload_len = get_u32(&logical, first + 16) as usize;
    assert_eq!(get_u32(&logical, first + 20), 0, "record reserved word");
    let trace_start = first + 24;
    assert_eq!(
        &logical[trace_start..trace_start + trace_len],
        records[0].trace_id.as_bytes(),
        "trace id"
    );
    assert_eq!(
        attribute_count,
        records[0].attributes.len(),
        "attribute count"
    );
    let dictionary = parse_dictionary(&bytes);
    let mut cursor = trace_start + trace_len;
    let mut last_id: Option<u32> = None;
    for (key, value) in &records[0].attributes {
        let key_id = get_u32(&logical, cursor);
        assert_eq!(
            dictionary[key_id as usize], *key,
            "the key id indexes the attribute section's dictionary"
        );
        assert!(
            last_id.is_none() || last_id < Some(key_id),
            "pairs are written in ascending key-id order"
        );
        last_id = Some(key_id);
        cursor += 4;
        assert_eq!(
            &logical[cursor..cursor + 16],
            hash_attribute(key, value).as_bytes(),
            "the pair digest is the same (key, value) digest the index posts under"
        );
        cursor += 16;
    }
    assert_eq!(
        &logical[cursor..cursor + payload_len],
        records[0].payload.as_slice(),
        "payload follows the attribute pairs"
    );

    // The point of the digest pairs is that the VALUE TEXT IS NOT THERE —
    // not in the record, not in the index, not anywhere in the file. That is
    // a property of the bytes, not of the reader. The two-byte "ok" is
    // searched only in the decoded logical text regions, where every byte is
    // structured; the longer values are searched across both whole files,
    // where digest and compressed bytes are effectively random.
    let attribute_section = &bytes
        [get_u64(&bytes, offsets::ATTRIBUTE_INDEX_OFFSET) as usize..attribute_index_end(&bytes)];
    for record in &records {
        for value in record.attributes.values() {
            assert!(
                !contains(attribute_section, value.as_bytes()),
                "attribute value {value:?} must not be stored in the index"
            );
            if value.len() >= 4 {
                assert!(
                    !contains(&logical, value.as_bytes()),
                    "attribute value {value:?} must not be stored in the record region"
                );
                assert!(
                    !contains(&bytes, value.as_bytes()) && !contains(&raw_file, value.as_bytes()),
                    "attribute value {value:?} must not appear anywhere in the file"
                );
            }
        }
    }

    // Finally: the hand-parsed view and the production parser must agree.
    let parsed = Segment::from_bytes(bytes.clone())
        .expect("the real reader accepts the real encoder's output");
    let header = parsed.header();
    assert_eq!(header.version, get_u16(&bytes, offsets::VERSION));
    assert_eq!(header.codec, Codec::Lz4);
    assert_eq!(header.record_count, get_u64(&bytes, offsets::RECORD_COUNT));
    assert_eq!(
        header.records_offset,
        get_u64(&bytes, offsets::RECORDS_OFFSET)
    );
    assert_eq!(header.records_len, get_u64(&bytes, offsets::RECORDS_LEN));
    assert_eq!(
        header.records_logical_len,
        get_u64(&bytes, offsets::RECORDS_LOGICAL_LEN)
    );
    assert_eq!(
        header.directory_offset,
        get_u64(&bytes, offsets::DIRECTORY_OFFSET)
    );
    assert_eq!(
        header.directory_len,
        get_u64(&bytes, offsets::DIRECTORY_LEN)
    );
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
            "codec_id",
            "record_count",
            "six_contiguous_sections",
            "sections_in_bounds",
            "no_trailing_bytes",
            "offset_index_length",
            "ascending_logical_record_offsets",
            "directory_arithmetic",
            "block_crc32",
            "record_aligned_blocks",
            "codec_independent_logical_bytes",
            "record_encoding_digest_pairs",
            "no_attribute_value_text_anywhere",
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

    // Attribute lookup discriminates on the exact key/value pair. The oracle
    // is the corpus: the records whose attributes hold the value, identified
    // by their distinct payloads.
    let checkout = parsed
        .query_attribute("service", "checkout")
        .expect("attribute query");
    assert_eq!(checkout.len(), 2, "two records carry service=checkout");
    assert!(
        parsed.last_query_used_index(),
        "attribute lookup must be index-served"
    );
    assert_eq!(
        checkout
            .iter()
            .map(|record| record.payload().to_vec())
            .collect::<Vec<_>>(),
        vec![b"first".to_vec(), b"second".to_vec()],
        "exactly the records the corpus gave service=checkout"
    );

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

    // Ordinal access preserves encode order, and every record carries the
    // digest pairs the format derives from its attributes.
    for (index, expected) in records.iter().enumerate() {
        let record = parsed
            .record(index)
            .expect("ordinal access")
            .expect("record present");
        assert_eq!(record.timestamp(), expected.timestamp);
        assert_eq!(record.trace_id(), expected.trace_id);
        assert_eq!(record.payload(), expected.payload.as_slice());
        assert_eq!(
            record.attribute_pairs(),
            expected_pairs(&records, expected).as_slice(),
            "the stored pairs are the derivation from the record's attributes"
        );
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
            "digest_pairs_round_trip",
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

    // The bytes on disk are exactly what the encoder produced for this input
    // — the determinism rule now covers the compressor's output too.
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
            "compressor_output_is_deterministic",
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
    // and indexes (the resident block directory included), never the record
    // payloads.
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

    // Querying decodes on demand and retains no record structs. What it DOES
    // retain — and must report rather than hide — is the decoded-block
    // cache: bounded per segment, and counted by `resident_bytes` so the
    // store's accounting cannot claim a busy segment holds nothing.
    let found = opened.query_trace("trace-a").expect("trace query");
    assert_eq!(found.len(), 2);
    assert_eq!(
        opened.resident_decoded_record_count(),
        0,
        "querying must not accumulate decoded records in the segment"
    );
    let cached = opened.resident_bytes();
    assert!(
        cached > 0,
        "the decoded-block cache is real residency and must be counted"
    );
    assert!(
        cached as u64 <= opened.header().records_logical_len,
        "the cache is bounded by the blocks it holds: {cached} bytes"
    );

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
            "block_cache_residency_is_counted",
            "offset_addressed_access",
            "timestamp_without_full_decode",
            "memory_backed_holds_bytes",
        ],
    );
}

#[test]
fn foreign_bytes_are_rejected() {
    // The reader must refuse anything that is not a current segment rather
    // than misinterpret it. Legacy v1 JSONL carried no magic at all.
    let legacy = b"{\"timestamp\":1700000000000000000,\"message\":\"legacy\"}\n".to_vec();
    assert_ne!(&legacy[0..8], b"TRAZASEG");
    assert!(
        Segment::from_bytes(legacy).is_err(),
        "a JSONL v1 segment must not parse as v7"
    );

    // A correct magic with the wrong version is refused too.
    let mut wrong_version = segment::encode(&corpus()).expect("encode");
    wrong_version[offsets::VERSION] = 99;
    assert!(
        Segment::from_bytes(wrong_version).is_err(),
        "an unsupported version must be refused"
    );

    // A correct version with an unknown codec id is refused the same way:
    // decoding blocks under the wrong codec would produce garbage, not
    // errors.
    let mut wrong_codec = segment::encode(&corpus()).expect("encode");
    wrong_codec[offsets::CODEC] = 9;
    let refusal = Segment::from_bytes(wrong_codec).expect_err("unknown codec is refused");
    assert!(
        refusal.to_string().contains("codec id 9"),
        "the refusal names the codec it found: {refusal}"
    );

    // A wrong magic is refused even when everything after it is valid.
    let mut wrong_magic = segment::encode(&corpus()).expect("encode");
    wrong_magic[0] = b'X';
    assert!(
        Segment::from_bytes(wrong_magic).is_err(),
        "a foreign magic must be refused"
    );

    // Sections must be contiguous: stretching the stored record region
    // leaves the next section starting somewhere other than where records
    // end.
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
            "legacy_jsonl_not_parsed",
            "unsupported_version_refused",
            "unknown_codec_refused_by_name",
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

/// Rewrites the attribute section so every entry posts every record — the
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
    // The index answers with candidates, so correctness rests entirely on
    // the reader checking each candidate against the record it points at.
    // Here every posting list names every record; a reader that trusted the
    // index would return all three rows for every query. In v7 the record
    // carries no value text, so the check is against the digest pairs the
    // record ITSELF stores — with the oracle here being the payload each
    // corpus record was given, and the store's parsed-payload verification
    // standing behind it in the query path.
    let records = corpus();
    let honest = segment::encode(&records).expect("encode");
    let forged = forge_digest_collisions(&honest, records.len());

    let segment = Segment::from_bytes(forged).expect("the forged segment still parses");

    let checkout = segment
        .query_attribute("service", "checkout")
        .expect("attribute query");
    // The payload-derived oracle: exactly the corpus records whose
    // attributes hold the value.
    let expected: Vec<Vec<u8>> = records
        .iter()
        .filter(|record| record.attributes.get("service").map(String::as_str) == Some("checkout"))
        .map(|record| record.payload.clone())
        .collect();
    assert_eq!(
        checkout
            .iter()
            .map(|record| record.payload().to_vec())
            .collect::<Vec<_>>(),
        expected,
        "verification must discard the candidates that do not hold the value"
    );
    assert_eq!(checkout.len(), 2);

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
            "payload_derived_oracle",
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
    // than an unknown one: it overlaps nothing and is always skippable. It
    // also carries an empty directory and a zero logical length.
    let empty_bytes = segment::encode(&[]).expect("encode empty");
    assert_eq!(get_u64(&empty_bytes, offsets::DIRECTORY_LEN), 0);
    assert_eq!(get_u64(&empty_bytes, offsets::RECORDS_LOGICAL_LEN), 0);
    let empty = Segment::from_bytes(empty_bytes).expect("opens");
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
    // 1 through 6 are covered because all six were written by real builds:
    // 1 was JSONL, 2 shipped in 0.16/0.17, 3 in 0.18/0.19, 4 and 5 existed
    // only on unreleased main, and 6 in every release before v0.24.0. Those
    // identifiers stay spent either way — reusing one for a different layout
    // would make a header declaring it ambiguous between two incompatible
    // files, which is the exact failure the field exists to prevent.
    let records = corpus();
    for stale in [1_u16, 2, 3, 4, 5, 6] {
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
    // Same discipline as the timestamp range: absent must read as UNKNOWN,
    // never as empty. A segment carrying no indexable text must be scanned.
    // Reading absence as "holds nothing" would make content search silently
    // return no rows.
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

/// Ascending record order is a format invariant the reader BINARY-SEARCHES on,
/// so the encoder has to establish it rather than trust the caller. A binary
/// search over unordered records does not fail — it returns the wrong ones.
#[test]
fn records_are_stored_in_ascending_timestamp_order_whatever_order_they_arrive_in() {
    let shuffled = vec![
        RecordInput::new(
            1_700_000_000_000_000_200,
            "trace-a",
            attributes(&[("service", "billing")]),
            b"third".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_000,
            "trace-a",
            attributes(&[("service", "checkout")]),
            b"first".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_100,
            "trace-b",
            attributes(&[("service", "checkout")]),
            b"second".to_vec(),
        ),
    ];
    let segment = Segment::from_bytes(segment::encode(&shuffled).expect("encode")).expect("opens");

    let stored: Vec<(u64, Vec<u8>)> = (0..segment.len())
        .map(|ordinal| {
            let record = segment
                .record(ordinal)
                .expect("ordinal access")
                .expect("record present");
            (record.timestamp(), record.payload().to_vec())
        })
        .collect();
    assert_eq!(
        stored,
        vec![
            (1_700_000_000_000_000_000, b"first".to_vec()),
            (1_700_000_000_000_000_100, b"second".to_vec()),
            (1_700_000_000_000_000_200, b"third".to_vec()),
        ],
        "the encoder sorts, so ordinal order is timestamp order"
    );

    // And the range search agrees with a brute-force filter at every bound,
    // including the empty, point, and past-the-end cases.
    let stamps: Vec<u64> = stored.iter().map(|(timestamp, _)| *timestamp).collect();
    let probes = [
        1_699_999_999_999_999_999,
        1_700_000_000_000_000_000,
        1_700_000_000_000_000_050,
        1_700_000_000_000_000_100,
        1_700_000_000_000_000_200,
        1_700_000_000_000_000_999,
    ];
    for since in probes {
        for until in probes {
            let range = segment
                .ordinal_range_for_window(Some(since), Some(until))
                .expect("range");
            let expected: Vec<usize> = stamps
                .iter()
                .enumerate()
                .filter(|(_, stamp)| **stamp >= since && **stamp <= until)
                .map(|(ordinal, _)| ordinal)
                .collect();
            assert_eq!(
                range.collect::<Vec<usize>>(),
                expected,
                "window [{since}, {until}] must select exactly the in-window ordinals"
            );
        }
    }

    // Unbounded on either side, and the saturating top bound.
    assert_eq!(
        segment
            .ordinal_range_for_window(None, None)
            .expect("range")
            .collect::<Vec<usize>>(),
        vec![0, 1, 2],
        "an unbounded window selects everything"
    );
    assert_eq!(
        segment
            .ordinal_range_for_window(Some(1_700_000_000_000_000_100), None)
            .expect("range")
            .collect::<Vec<usize>>(),
        vec![1, 2]
    );
    assert_eq!(
        segment
            .ordinal_range_for_window(None, Some(u64::MAX))
            .expect("range")
            .collect::<Vec<usize>>(),
        vec![0, 1, 2],
        "u64::MAX as an upper bound must select the tail, not wrap to nothing"
    );

    evidence(
        "record_order",
        &[
            "encoder_sorts_by_timestamp",
            "range_search_matches_brute_force",
            "unbounded_and_saturating_bounds",
        ],
    );
}

/// Deterministic pseudo-random bytes: incompressible input for the
/// raw-passthrough path without an RNG dependency.
fn incompressible_bytes(mut state: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push((state >> 33) as u8);
    }
    out
}

/// A corpus that exercises the whole carving space: an incompressible record
/// larger than one block, a compressible record larger than one block, and
/// two small records that share a block.
fn carving_corpus() -> Vec<RecordInput> {
    let oversize = COMPRESSION_BLOCK_BYTES + 64 * 1024;
    vec![
        RecordInput::new(
            1_700_000_000_000_000_000,
            "trace-random",
            attributes(&[("service", "noise")]),
            incompressible_bytes(7, oversize),
        ),
        RecordInput::new(
            1_700_000_000_000_000_100,
            "trace-repeat",
            attributes(&[("service", "repeat")]),
            vec![b'a'; oversize],
        ),
        RecordInput::new(
            1_700_000_000_000_000_200,
            "trace-small",
            attributes(&[("service", "small")]),
            b"tail-one".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_300,
            "trace-small",
            attributes(&[("service", "small")]),
            b"tail-two".to_vec(),
        ),
    ]
}

#[test]
fn blocks_are_carved_at_record_boundaries_with_raw_passthrough() {
    let records = carving_corpus();
    let bytes = segment::encode(&records).expect("encode");
    let directory = parse_directory(&bytes);

    // Three blocks: each oversized record alone (no record spans blocks, so
    // a record bigger than the target IS a block), then both small records
    // sharing the tail block.
    assert_eq!(
        directory.len(),
        3,
        "two oversized records and one shared tail block"
    );
    assert_eq!(
        get_u64(&bytes, offsets::DIRECTORY_LEN),
        3 * DIRECTORY_ENTRY_LEN as u64
    );

    // The incompressible block trips the raw-passthrough flag: compression
    // that does not strictly shrink is discarded, so already-compressed
    // payload text cannot make the stored region larger than the logical
    // one.
    assert!(
        directory[0].raw,
        "an incompressible block is stored raw and flagged"
    );
    let extent_0 = directory[1].logical_start - directory[0].logical_start;
    assert_eq!(
        u64::from(directory[0].stored_len),
        extent_0,
        "a raw block stores exactly its logical bytes"
    );

    // The repetitive block compresses, so it is stored smaller and unflagged.
    assert!(
        !directory[1].raw,
        "a compressible block is stored compressed"
    );
    let extent_1 = directory[2].logical_start - directory[1].logical_start;
    assert!(
        u64::from(directory[1].stored_len) < extent_1,
        "the compressed block is strictly smaller than its logical extent"
    );

    // The tail block's fence is its first record's timestamp.
    assert_eq!(directory[2].min_timestamp, 1_700_000_000_000_000_200);

    // Round trip through the real reader, both backings.
    let opened = Segment::from_bytes(bytes.clone()).expect("byte-resident open");
    for (ordinal, expected) in records.iter().enumerate() {
        let record = opened
            .record(ordinal)
            .expect("ordinal access")
            .expect("record present");
        assert_eq!(record.payload(), expected.payload.as_slice());
        assert_eq!(record.timestamp(), expected.timestamp);
    }
    let directory_path = temp_dir("carving");
    let path = directory_path.join("segment-00000000000000000000.seg");
    fs::write(&path, &bytes).expect("write segment");
    let reopened = Segment::open(&path).expect("file-backed open");
    assert_eq!(
        reopened.query_trace("trace-random").expect("trace query")[0].payload(),
        records[0].payload.as_slice(),
        "an oversized raw block round-trips through the file backing"
    );
    assert_eq!(
        reopened.query_trace("trace-repeat").expect("trace query")[0].payload(),
        records[1].payload.as_slice(),
        "an oversized compressed block round-trips through the file backing"
    );
    let window = reopened
        .query_time_range(1_700_000_000_000_000_100, 1_700_000_000_000_000_200)
        .expect("window");
    assert_eq!(
        window.len(),
        2,
        "the window search crosses block boundaries"
    );

    cleanup(&directory_path);
    evidence(
        "block_carving",
        &[
            "oversized_record_is_its_own_block",
            "raw_passthrough_flagged",
            "raw_block_length_equals_extent",
            "compressed_block_strictly_smaller",
            "block_min_timestamp_fence",
            "round_trip_both_backings",
            "window_search_across_blocks",
        ],
    );
}

#[test]
fn codec_zero_still_carves_blocks_and_reads_identically() {
    // Codec 0 is a codec choice, not a format variant: same carving, same
    // directory, same CRCs, logical and stored offsets coinciding — one
    // reader shape.
    let records = carving_corpus();
    let raw_file = segment::encode_with_codec(&records, true, Codec::Raw).expect("encode raw");
    assert_eq!(get_u32(&raw_file, offsets::CODEC), 0, "codec id 0");
    assert_eq!(
        get_u64(&raw_file, offsets::RECORDS_LEN),
        get_u64(&raw_file, offsets::RECORDS_LOGICAL_LEN),
        "logical and physical lengths coincide under codec 0"
    );
    let directory = parse_directory(&raw_file);
    assert_eq!(directory.len(), 3, "carving is codec-independent");
    for (index, entry) in directory.iter().enumerate() {
        assert_eq!(
            entry.logical_start, entry.stored_offset,
            "block {index}: logical and stored offsets coincide under codec 0"
        );
    }

    // The reader answers identically under both codecs.
    let raw_segment = Segment::from_bytes(raw_file).expect("raw-codec segment opens");
    let lz4_segment =
        Segment::from_bytes(segment::encode(&records).expect("encode lz4")).expect("opens");
    for ordinal in 0..records.len() {
        assert_eq!(
            raw_segment
                .record(ordinal)
                .expect("raw ordinal")
                .expect("present"),
            lz4_segment
                .record(ordinal)
                .expect("lz4 ordinal")
                .expect("present"),
            "record {ordinal} reads identically under both codecs"
        );
    }

    evidence(
        "codec_zero",
        &[
            "codec_zero_carves_and_directs",
            "logical_equals_stored_offsets",
            "identical_reads_across_codecs",
        ],
    );
}

#[test]
fn the_records_region_compresses_a_repetitive_corpus() {
    // The storage gate proper is measured by storage-bench; this is the
    // format-level floor: on an openly repetitive corpus the records region
    // must shrink by a solid multiple, or the compression machinery is
    // decorative. 2.5x is deliberately generous against LZ4 on templated
    // text.
    let records: Vec<RecordInput> = (0..2_000)
        .map(|index| {
            RecordInput::new(
                1_700_000_000_000_000_000 + index,
                format!("trace-{index}"),
                attributes(&[("service", "assistant"), ("status", "ok")]),
                format!(
                    "{{\"prompt\":\"You are a helpful assistant. Answer the customer's \
                     question about their order politely and concisely.\",\"turn\":{index},\
                     \"completion\":\"Thank you for reaching out about your order. It is on \
                     its way and should arrive shortly.\"}}"
                )
                .into_bytes(),
            )
        })
        .collect();
    let bytes = segment::encode(&records).expect("encode");
    let stored = get_u64(&bytes, offsets::RECORDS_LEN) as f64;
    let logical = get_u64(&bytes, offsets::RECORDS_LOGICAL_LEN) as f64;
    assert!(
        logical > COMPRESSION_BLOCK_BYTES as f64,
        "the corpus spans multiple blocks, so the ratio is measured across block cuts"
    );
    let ratio = logical / stored;
    println!(
        "records region compression on the repetitive corpus: {ratio:.2}x \
         ({logical:.0} logical bytes stored as {stored:.0})"
    );
    assert!(
        ratio >= 2.5,
        "records region must compress at least 2.5x on a repetitive corpus, got {ratio:.2}x \
         ({logical:.0} logical over {stored:.0} stored)"
    );

    // And it still reads: spot-check a round trip through the block decode.
    let opened = Segment::from_bytes(bytes).expect("opens");
    assert_eq!(opened.len(), records.len());
    assert_eq!(
        opened.query_trace("trace-1999").expect("trace")[0].payload(),
        records[1_999].payload.as_slice()
    );

    evidence(
        "compression",
        &["multi_block_corpus", "ratio_at_least_2_5x", "round_trip"],
    );
}

#[test]
fn a_flipped_stored_byte_is_refused_by_the_block_crc() {
    let records = carving_corpus();
    let honest = segment::encode(&records).expect("encode");

    // Flip one byte inside the stored records region. The CRC is checked
    // BEFORE decode, so the failure is this error — never a decoder panic,
    // never garbage records.
    let records_offset = get_u64(&honest, offsets::RECORDS_OFFSET) as usize;
    let mut corrupted = honest.clone();
    corrupted[records_offset + 10] ^= 0xff;
    let refusal =
        Segment::from_bytes(corrupted.clone()).expect_err("the eager open checks every block");
    assert!(
        refusal.to_string().contains("crc32"),
        "the refusal names the checksum: {refusal}"
    );

    // The file-backed open decodes the FIRST and LAST blocks for the
    // exact-bounds validation (spec amendment #5), so damage in either is
    // caught at open by the same checksum.
    let directory = temp_dir("crc-mutation");
    let path = directory.join("segment-00000000000000000000.seg");
    fs::write(&path, &corrupted).expect("write corrupted segment");
    let open_refusal =
        Segment::open(&path).expect_err("the open-time validation decodes the first block");
    assert!(
        open_refusal.to_string().contains("crc32"),
        "the open-time refusal names the checksum: {open_refusal}"
    );

    // A MIDDLE block stays lazy — no open-time decode touches it — so the
    // same damage there surfaces at the first read that touches the block.
    let dir_offset = get_u64(&honest, offsets::DIRECTORY_OFFSET) as usize;
    let middle_stored_offset = get_u64(&honest, dir_offset + DIRECTORY_ENTRY_LEN + 8) as usize;
    let mut middle_corrupted = honest.clone();
    middle_corrupted[records_offset + middle_stored_offset + 10] ^= 0xff;
    let middle_path = directory.join("segment-00000000000000000002.seg");
    fs::write(&middle_path, &middle_corrupted).expect("write middle-corrupted segment");
    let opened = Segment::open(&middle_path).expect("structural open succeeds");
    let read = opened.query_trace("trace-repeat").expect_err("read fails");
    assert!(
        read.to_string().contains("crc32"),
        "the lazy read names the checksum too: {read}"
    );

    // Flipping a directory entry is refused at open by the mandatory
    // directory validation — before any block is read.
    let mut forged_directory = honest.clone();
    forged_directory[dir_offset + 16] ^= 0x01; // stored-length word, entry 0
    let refusal = Segment::from_bytes(forged_directory.clone())
        .expect_err("a tampered directory entry is refused");
    assert!(
        refusal.to_string().contains("corrupt segment"),
        "directory tampering reads as corruption: {refusal}"
    );
    let forged_path = directory.join("segment-00000000000000000001.seg");
    fs::write(&forged_path, &forged_directory).expect("write forged segment");
    assert!(
        Segment::open(&forged_path).is_err(),
        "the file-backed open validates the directory eagerly"
    );

    cleanup(&directory);
    evidence(
        "mutation",
        &[
            "stored_byte_flip_fails_crc_eagerly",
            "first_block_flip_fails_crc_at_lazy_open",
            "middle_block_flip_fails_crc_lazily",
            "directory_flip_refused_at_open",
        ],
    );
}

/// The derivation invariant (format v7): for every record a REAL store
/// writes, applying the span → record derivation to the parsed payload must
/// reproduce the stored `(key id, digest)` list exactly. This is what makes
/// the digest list droppable text rather than lost data — and this test
/// re-derives it independently: canonical JSON for user attributes, raw text
/// for the reserved service/name/tenant keys, NUL-prefixed user keys
/// excluded.
#[test]
fn stored_digest_pairs_are_rederivable_from_every_payload() {
    let directory = temp_dir("derivation");
    let store = traza::Store::open(&directory, traza::Config::default()).expect("store opens");

    // A deterministic pseudo-randomized corpus over the derivation's whole
    // input space: string/number/bool/array/object values, tenants present
    // and absent, and a NUL-prefixed key that must NOT be indexed.
    let mut state = 42u64;
    let mut next = |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    let span_count = 64u64;
    for index in 0..span_count {
        let value: serde_json::Value = match next(5) {
            0 => serde_json::Value::String(format!("text-{}", next(10))),
            1 => serde_json::Value::from(next(1_000)),
            2 => serde_json::Value::Bool(next(2) == 0),
            3 => serde_json::json!([1, "two", {"three": 3}]),
            _ => serde_json::json!({"nested": {"deep": true}, "n": 7}),
        };
        let tenant = if next(3) == 0 { "acme" } else { "" };
        let mut span_json = serde_json::json!({
            "trace_id": format!("trace-{}", next(8)),
            "span_id": format!("span-{index}"),
            "name": format!("op-{}", next(4)),
            "service": format!("svc-{}", next(3)),
            "start_time_ns": 1_700_000_000_000_000_000u64 + index,
            "end_time_ns": 1_700_000_000_000_000_500u64 + index,
            "attributes": {
                "model": format!("model-{}", next(4)),
                "tokens": next(4_096),
                "mixed": value,
                "\u{0}service": "poison-attempt",
            },
        });
        if !tenant.is_empty() {
            span_json["$tenant"] = serde_json::Value::String(tenant.to_owned());
        }
        let span: traza::Span = serde_json::from_value(span_json).expect("span");
        store.ingest(span).expect("ingest");
    }
    store.flush().expect("flush seals a segment");
    drop(store);

    let segment_path = fs::read_dir(&directory)
        .expect("read store directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("the flush produced a segment");
    let bytes = fs::read(&segment_path).expect("read segment bytes");
    assert_eq!(get_u16(&bytes, offsets::VERSION), VERSION);

    let dictionary = parse_dictionary(&bytes);
    let logical = inflate_records_region(&bytes);
    let offsets_offset = get_u64(&bytes, offsets::OFFSETS_OFFSET) as usize;
    let record_count = get_u64(&bytes, offsets::RECORD_COUNT);
    assert_eq!(record_count, span_count, "every span was sealed");

    for ordinal in 0..record_count as usize {
        let start = get_u64(&bytes, offsets_offset + ordinal * 8) as usize;
        let trace_len = get_u32(&logical, start + 8) as usize;
        let attribute_count = get_u32(&logical, start + 12) as usize;
        let payload_len = get_u32(&logical, start + 16) as usize;
        let mut cursor = start + 24 + trace_len;
        let stored_pairs: Vec<(u32, [u8; 16])> = (0..attribute_count)
            .map(|_| {
                let id = get_u32(&logical, cursor);
                let mut digest = [0u8; 16];
                digest.copy_from_slice(&logical[cursor + 4..cursor + 20]);
                cursor += 20;
                (id, digest)
            })
            .collect();
        let payload: serde_json::Value =
            serde_json::from_slice(&logical[cursor..cursor + payload_len])
                .expect("the payload is the span's JSON");

        // The derivation, re-implemented from its definition: user
        // attributes minus NUL-prefixed keys, canonicalized to JSON text;
        // the reserved keys carrying RAW text; only a non-empty tenant.
        let mut derived: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in payload["attributes"].as_object().expect("attributes") {
            if !key.starts_with('\u{0}') {
                derived.insert(
                    key.clone(),
                    serde_json::to_string(value).expect("canonical json"),
                );
            }
        }
        derived.insert(
            "\u{0}service".to_owned(),
            payload["service"].as_str().expect("service").to_owned(),
        );
        derived.insert(
            "\u{0}name".to_owned(),
            payload["name"].as_str().expect("name").to_owned(),
        );
        if let Some(tenant) = payload.get("$tenant").and_then(|value| value.as_str()) {
            if !tenant.is_empty() {
                derived.insert("\u{0}tenant".to_owned(), tenant.to_owned());
            }
        }
        let expected: Vec<(u32, [u8; 16])> = derived
            .iter()
            .map(|(key, value)| {
                let id = dictionary
                    .iter()
                    .position(|held| held == key)
                    .unwrap_or_else(|| panic!("key {key:?} missing from the dictionary"))
                    as u32;
                (id, *hash_attribute(key, value).as_bytes())
            })
            .collect();
        assert_eq!(
            stored_pairs, expected,
            "record {ordinal}: the stored pair list must equal the payload-derived one, \
             byte for byte"
        );
    }

    cleanup(&directory);
    evidence(
        "derivation_invariant",
        &[
            "fresh_store_writes_v7",
            "pairs_rederived_from_parsed_payloads",
            "reserved_keys_raw_text",
            "user_attributes_canonical_json",
            "nul_prefixed_keys_excluded",
        ],
    );
}

#[test]
fn a_forged_records_logical_length_is_refused_not_allocated() {
    // Reproduces the review probe. Header offset 120 (the records LOGICAL
    // length) carries no checksum, and before the block-extent bounds it
    // sized the last block's decompress allocation directly: 1<<62 aborted
    // the process ("memory allocation of 4611686018427387904 bytes failed",
    // SIGABRT — no error, and a store that crash-loops at every open), and
    // 1<<30 zero-filled a gigabyte before failing. Both must be a named
    // corrupt-segment refusal at open, with no allocation at all.
    let records = corpus();
    let honest = segment::encode(&records).expect("encode");

    for forged_len in [1u64 << 62, 1u64 << 30] {
        let mut forged = honest.clone();
        forged[offsets::RECORDS_LOGICAL_LEN..offsets::RECORDS_LOGICAL_LEN + 8]
            .copy_from_slice(&forged_len.to_le_bytes());
        let refusal = Segment::from_bytes(forged.clone())
            .expect_err("a forged logical length must refuse, never allocate");
        assert!(
            refusal.to_string().contains("block logical extent"),
            "the refusal names the bound (forged {forged_len}): {refusal}"
        );

        // The file-backed open runs the same directory validation eagerly,
        // so a crafted file cannot defer the abort to the first query.
        let directory = temp_dir("logical-len");
        let path = directory.join("segment-00000000000000000000.seg");
        fs::write(&path, &forged).expect("write forged segment");
        let refusal =
            Segment::open(&path).expect_err("the file-backed open refuses the forged length");
        assert!(
            refusal.to_string().contains("block logical extent"),
            "the lazy open refuses by the same bound: {refusal}"
        );
        cleanup(&directory);
    }

    // A single-record block is bounded by the record bound rather than the
    // 128 KiB target, so a forged length below 2^31 slips that check — and
    // must then be caught by LZ4's expansion ceiling: a compressed block
    // cannot legally inflate past ~256x its stored bytes.
    let tail_heavy = vec![
        RecordInput::new(
            1_700_000_000_000_000_000,
            "trace-small",
            attributes(&[("service", "small")]),
            b"tiny".to_vec(),
        ),
        RecordInput::new(
            1_700_000_000_000_000_100,
            "trace-repeat",
            attributes(&[("service", "repeat")]),
            vec![b'a'; COMPRESSION_BLOCK_BYTES + 64 * 1024],
        ),
    ];
    let bytes = segment::encode(&tail_heavy).expect("encode");
    let entries = parse_directory(&bytes);
    assert!(
        !entries.last().expect("entries").raw,
        "the oversized repetitive record compresses, so the expansion bound applies"
    );
    let mut forged = bytes.clone();
    forged[offsets::RECORDS_LOGICAL_LEN..offsets::RECORDS_LOGICAL_LEN + 8]
        .copy_from_slice(&(1u64 << 30).to_le_bytes());
    let refusal = Segment::from_bytes(forged)
        .expect_err("a sub-2^31 forgery on a single-record block must still refuse");
    assert!(
        refusal.to_string().contains("lz4"),
        "the refusal names the expansion ceiling: {refusal}"
    );

    // And the honest files still open: the bounds reject forgeries, not
    // legal segments.
    Segment::from_bytes(honest).expect("the honest corpus still opens");
    Segment::from_bytes(bytes).expect("the honest oversized-record segment still opens");

    evidence(
        "allocation_bounds",
        &[
            "forged_logical_length_refused_eagerly",
            "forged_logical_length_refused_file_backed",
            "single_record_block_bounded_by_lz4_expansion",
            "honest_segments_unaffected",
        ],
    );
}

#[test]
fn a_forged_block_min_timestamp_is_refused_not_believed() {
    // The directory's min-timestamp fences steer the window search without
    // decoding anything, and no checksum covers them. Review reproduced the
    // consequence: nudging one fence while keeping the column sorted made
    // `query_time_range` return out-of-window records — or silently drop
    // in-window ones — with no error anywhere. The fences are now verified
    // against the records themselves: every decoded block's first record
    // must carry its fence (which makes the byte-resident open, with its
    // eager decode, an open-time check), and both landed window bounds are
    // confirmed against the records on each side.
    let records = carving_corpus();
    let honest = segment::encode(&records).expect("encode");
    let entries = parse_directory(&honest);
    assert!(entries.len() >= 3, "the forgery needs interior fences");

    // The hand-check on the honest file: every fence IS the timestamp of
    // its block's first record.
    assert_eq!(entries[0].min_timestamp, records[0].timestamp);
    assert_eq!(entries[1].min_timestamp, records[1].timestamp);
    assert_eq!(entries[2].min_timestamp, records[2].timestamp);

    // Nudge block 1's fence up by 50ns: the column stays sorted, so every
    // pre-existing open-time check still passes.
    let dir_offset = get_u64(&honest, offsets::DIRECTORY_OFFSET) as usize;
    let fence_offset = dir_offset + DIRECTORY_ENTRY_LEN + 24;
    let forged_fence = entries[1].min_timestamp + 50;
    assert!(forged_fence < entries[2].min_timestamp, "still sorted");
    let mut forged = honest.clone();
    forged[fence_offset..fence_offset + 8].copy_from_slice(&forged_fence.to_le_bytes());

    let refusal = Segment::from_bytes(forged.clone())
        .expect_err("the eager open verifies every fence against its block");
    assert!(
        refusal.to_string().contains("min timestamp"),
        "the refusal names the fence: {refusal}"
    );

    // The file-backed open reads no blocks, so the forged file opens — and
    // the window query the forgery would have shifted answers Corrupt
    // instead of wrong rows. This exact query silently dropped record 1
    // before the fences were verified.
    let directory = temp_dir("fence-mutation");
    let path = directory.join("segment-00000000000000000000.seg");
    fs::write(&path, &forged).expect("write forged segment");
    let opened = Segment::open(&path).expect("the lazy open cannot see block contents");
    let error = opened
        .query_time_range(records[0].timestamp, entries[1].min_timestamp)
        .expect_err("a window landing on the forged fence must refuse");
    assert!(
        error.to_string().contains("min timestamp"),
        "the window search names the fence disagreement: {error}"
    );

    cleanup(&directory);
    evidence(
        "min_timestamp_fences",
        &[
            "honest_fences_match_first_records",
            "forged_fence_refused_at_eager_open",
            "forged_fence_refused_at_window_query",
        ],
    );
}

/// Derives a payload's attribute map exactly as `span_to_record` defines it:
/// user attributes minus NUL-prefixed keys canonicalized to JSON text, the
/// reserved service/name keys carrying raw text, tenant only when non-empty.
fn payload_derived_attributes(payload: &[u8]) -> BTreeMap<String, String> {
    let parsed: serde_json::Value = serde_json::from_slice(payload).expect("span payload");
    let mut derived = BTreeMap::new();
    for (key, value) in parsed["attributes"].as_object().expect("attributes") {
        if !key.starts_with('\u{0}') {
            derived.insert(
                key.clone(),
                serde_json::to_string(value).expect("canonical json"),
            );
        }
    }
    derived.insert(
        "\u{0}service".to_owned(),
        parsed["service"].as_str().expect("service").to_owned(),
    );
    derived.insert(
        "\u{0}name".to_owned(),
        parsed["name"].as_str().expect("name").to_owned(),
    );
    if let Some(tenant) = parsed.get("$tenant").and_then(|value| value.as_str()) {
        if !tenant.is_empty() {
            derived.insert("\u{0}tenant".to_owned(), tenant.to_owned());
        }
    }
    derived
}

#[test]
fn a_forged_record_digest_pair_is_caught_by_the_store_layer() {
    // Amendment #2 moved the collision authority: the segment layer confirms
    // a candidate against the digest pairs the record ITSELF carries, so an
    // adversary who plants a colliding pair in BOTH the record bytes and the
    // index defeats the segment layer entirely — which is exactly the shape
    // a true 128-bit collision has. The store's verification against the
    // parsed payload is the only defense left standing, and the
    // posting-only forgery in `a_digest_collision_cannot_produce_a_wrong_row`
    // never exercises it. This test forges the full scenario and proves the
    // store layer holds: the forged row must vanish from store answers while
    // the segment layer, honestly, admits it.
    let seal_dir = temp_dir("collision-seal");
    let store = traza::Store::open(&seal_dir, traza::Config::default()).expect("store opens");
    for (index, service) in ["checkout", "checkout", "billing"].iter().enumerate() {
        let span: traza::Span = serde_json::from_value(serde_json::json!({
            "trace_id": format!("trace-{index}"),
            "span_id": format!("span-{index}"),
            "name": "op",
            "service": service,
            "start_time_ns": 1_700_000_000_000_000_000u64 + index as u64,
            "end_time_ns": 1_700_000_000_000_000_500u64 + index as u64,
            "attributes": {"marker": format!("m{index}")},
        }))
        .expect("span");
        store.ingest(span).expect("ingest");
    }
    store.flush().expect("flush seals a segment");
    drop(store);
    let sealed_path = fs::read_dir(&seal_dir)
        .expect("read store directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("a sealed segment");

    // Re-encode the sealed records under the raw codec, so record bytes sit
    // at their logical offsets and the forgery can be written in place.
    let sealed = Segment::open(&sealed_path).expect("open sealed segment");
    let mut raw_records = Vec::with_capacity(sealed.len());
    for ordinal in 0..sealed.len() {
        let record = sealed.record(ordinal).expect("decodes").expect("present");
        raw_records.push(RecordInput::new(
            record.timestamp(),
            record.trace_id().to_owned(),
            payload_derived_attributes(record.payload()),
            record.payload().to_vec(),
        ));
    }
    drop(sealed);
    let bytes = segment::encode_with_codec(&raw_records, false, Codec::Raw).expect("raw encode");

    // Overwrite the billing record's service digest with the digest of
    // ("\0service", "checkout"): the record now CARRIES the colliding pair.
    let dictionary = parse_dictionary(&bytes);
    let service_id = dictionary
        .iter()
        .position(|key| key == "\u{0}service")
        .expect("service key in dictionary") as u32;
    let records_offset = get_u64(&bytes, offsets::RECORDS_OFFSET) as usize;
    let offsets_offset = get_u64(&bytes, offsets::OFFSETS_OFFSET) as usize;
    let record_count = get_u64(&bytes, offsets::RECORD_COUNT) as usize;
    let colliding = *hash_attribute("\u{0}service", "checkout").as_bytes();
    let mut forged = bytes.clone();
    let mut patched = false;
    for ordinal in 0..record_count {
        let start = records_offset + get_u64(&bytes, offsets_offset + ordinal * 8) as usize;
        let trace_len = get_u32(&bytes, start + 8) as usize;
        let attribute_count = get_u32(&bytes, start + 12) as usize;
        let payload_len = get_u32(&bytes, start + 16) as usize;
        let pairs_start = start + 24 + trace_len;
        let payload_start = pairs_start + attribute_count * 20;
        if !contains(
            &bytes[payload_start..payload_start + payload_len],
            b"billing",
        ) {
            continue;
        }
        for pair in 0..attribute_count {
            let at = pairs_start + pair * 20;
            if get_u32(&bytes, at) == service_id {
                forged[at + 4..at + 20].copy_from_slice(&colliding);
                patched = true;
            }
        }
    }
    assert!(patched, "the billing record's service digest was forged");

    // Re-seal the tampering: recompute every block CRC over the patched
    // stored bytes (raw codec: stored bytes ARE the logical bytes), then
    // forge the index postings too, as the posting-only test does.
    let dir_offset = get_u64(&forged, offsets::DIRECTORY_OFFSET) as usize;
    let entries = parse_directory(&forged);
    for (index, entry) in entries.iter().enumerate() {
        let stored_start = records_offset + entry.stored_offset as usize;
        let crc = crc32_ieee(&forged[stored_start..stored_start + entry.stored_len as usize])
            .to_le_bytes();
        let at = dir_offset + index * DIRECTORY_ENTRY_LEN + 20;
        forged[at..at + 4].copy_from_slice(&crc);
    }
    let forged = forge_digest_collisions(&forged, record_count);

    // The segment layer, by design, admits the forged row: confirmation is
    // a digest compare against pairs the record itself carries.
    let seg = Segment::from_bytes(forged.clone()).expect("the forged segment parses");
    assert_eq!(
        seg.query_attribute("\u{0}service", "checkout")
            .expect("segment query")
            .len(),
        3,
        "the segment layer is digest-confirmed and cannot see through a carried collision"
    );
    drop(seg);

    // The store can: every answer it returns is verified against the parsed
    // payload, which no digest forgery can satisfy.
    let store_dir = temp_dir("collision-store");
    fs::write(store_dir.join("segment-00000000000000000001.seg"), &forged)
        .expect("plant the forged segment");
    let store = traza::Store::open(&store_dir, traza::Config::default()).expect("store opens");
    let spans = store
        .query(&traza::SpanFilter {
            service: Some("checkout".to_owned()),
            ..traza::SpanFilter::default()
        })
        .expect("store query");
    assert_eq!(
        spans.len(),
        2,
        "the store returns only rows whose parsed payload holds the value: {spans:?}"
    );
    assert!(
        spans.iter().all(|span| span.service == "checkout"),
        "no forged row survives the payload-derived verification"
    );
    drop(store);

    cleanup(&seal_dir);
    cleanup(&store_dir);
    evidence(
        "collision_safety",
        &[
            "record_pairs_and_index_both_forged",
            "segment_layer_admits_carried_collision",
            "store_layer_rejects_by_parsed_payload",
        ],
    );
}

/// External review of v0.24.1, finding 3, the store-level harm: the header's
/// timestamp range was read at open and trusted by `may_contain_timestamps`
/// before any validation, so editing sixteen uncovered header bytes hid a
/// whole segment from time-bounded queries. A record the store could still
/// decode was silently omitted — the one outcome the corruption contract
/// forbids: corruption causes an error or a conservative scan, never a
/// false-negative predicate.
#[test]
fn a_forged_header_timestamp_range_cannot_silently_hide_records() {
    let directory = temp_dir("forged-header-range");
    let config = traza::Config {
        durability: traza::Durability::Buffered,
        compaction: None,
        ..traza::Config::default()
    };
    let store = traza::Store::open(&directory, config.clone()).expect("store opens");
    let span: traza::Span = serde_json::from_value(serde_json::json!({
        "trace_id": "t", "span_id": "s", "name": "op", "service": "svc",
        "start_time_ns": 100u64, "end_time_ns": 200u64,
    }))
    .expect("span parses");
    store.ingest(span).expect("span ingests");
    store.flush().expect("store flushes");
    drop(store);

    // Forge the range to [1000, 2000]: the record at 100 is now outside
    // what its own segment declares.
    let segment_path = fs::read_dir(&directory)
        .expect("store directory lists")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("the flush sealed a segment");
    let mut bytes = fs::read(&segment_path).expect("segment reads");
    bytes[offsets::MIN_TIMESTAMP..offsets::MIN_TIMESTAMP + 8]
        .copy_from_slice(&1_000u64.to_le_bytes());
    bytes[offsets::MAX_TIMESTAMP..offsets::MAX_TIMESTAMP + 8]
        .copy_from_slice(&2_000u64.to_le_bytes());
    fs::write(&segment_path, &bytes).expect("tampered segment writes");

    // The contract admits an error at open, an error at query, or the
    // record itself — never a clean empty answer.
    match traza::Store::open(&directory, config) {
        Err(refusal) => {
            let text = refusal.to_string();
            assert!(
                text.contains("corrupt"),
                "the open-time refusal names corruption: {text}"
            );
        }
        Ok(store) => {
            let filter = traza::SpanFilter {
                since_ns: Some(100),
                until_ns: Some(100),
                ..traza::SpanFilter::default()
            };
            match store.query(&filter) {
                Err(_) => {}
                Ok(spans) => assert!(
                    spans.iter().any(|span| span.span_id == "s"),
                    "a decodable record was silently omitted by the forged header range"
                ),
            }
        }
    }
    cleanup(&directory);
    evidence("mutation", &["forged_header_range_never_a_false_negative"]);
}

/// The segment-level halves of the same finding, stated as the amended
/// spec states them: BOTH opens validate BOTH bounds exactly — the lazy
/// open by decoding the first block (whose fence-vs-first-record check
/// makes the pinned min exact) and reading the last record's timestamp
/// (the true max, records being timestamp-sorted), the eager open against
/// every record — a record that decodes outside the declared range is
/// corrupt, and an empty segment must declare the one canonical empty
/// range.
#[test]
fn header_timestamp_forgeries_are_corrupt_not_silently_pruned() {
    let records = corpus(); // timestamps 1_700_000_000_000_000_000 .. +200
    let honest = segment::encode(&records).expect("encode");
    let true_min = get_u64(&honest, offsets::MIN_TIMESTAMP);
    let true_max = get_u64(&honest, offsets::MAX_TIMESTAMP);
    let directory = temp_dir("header-range-forgeries");
    let set_u64 = |bytes: &mut [u8], offset: usize, value: u64| {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };

    // A forged min disagrees with the first block fence: both opens refuse.
    let mut forged_min = honest.clone();
    set_u64(&mut forged_min, offsets::MIN_TIMESTAMP, true_min + 1);
    let eager = Segment::from_bytes(forged_min.clone()).expect_err("eager open refuses");
    assert!(
        eager.to_string().contains("min timestamp"),
        "the refusal names the min bound: {eager}"
    );
    let min_path = directory.join("segment-00000000000000000000.seg");
    fs::write(&min_path, &forged_min).expect("forged-min segment writes");
    assert!(
        Segment::open(&min_path).is_err(),
        "the lazy open validates the min bound against the first fence"
    );

    // A forged max below the last block fence is impossible for any
    // encoder output: both opens refuse.
    let mut forged_low_max = honest.clone();
    set_u64(&mut forged_low_max, offsets::MAX_TIMESTAMP, true_min - 1);
    assert!(
        Segment::from_bytes(forged_low_max.clone()).is_err(),
        "the eager open refuses a max below the last fence"
    );
    let low_max_path = directory.join("segment-00000000000000000001.seg");
    fs::write(&low_max_path, &forged_low_max).expect("forged-low-max segment writes");
    assert!(
        Segment::open(&low_max_path).is_err(),
        "the lazy open refuses a max below the last fence"
    );

    // A forged max BETWEEN the last fence and the true max used to be a
    // lazy-open residual (fences carry minima only, so no fence exposed
    // it), and it silently pruned tail windows for as long as no read
    // touched the hidden record. Both opens refuse it now: the eager open
    // against every record, the lazy open by reading the last record's
    // timestamp — the true max — which the forged bound cannot hold.
    let mut forged_mid_max = honest.clone();
    set_u64(&mut forged_mid_max, offsets::MAX_TIMESTAMP, true_max - 1);
    let eager = Segment::from_bytes(forged_mid_max.clone())
        .expect_err("the eager open validates the exact max");
    assert!(
        eager.to_string().contains("timestamp"),
        "the eager refusal names the range: {eager}"
    );
    let mid_max_path = directory.join("segment-00000000000000000002.seg");
    fs::write(&mid_max_path, &forged_mid_max).expect("forged-mid-max segment writes");
    let lazy = Segment::open(&mid_max_path)
        .expect_err("the lazy open reads the last record and refuses the forged max");
    assert!(
        lazy.to_string().contains("outside the header"),
        "the refusal names the range violation: {lazy}"
    );

    // An empty segment must declare the canonical empty range, nothing else.
    let empty = segment::encode(&[]).expect("empty encode");
    assert_eq!(get_u64(&empty, offsets::MIN_TIMESTAMP), u64::MAX);
    assert_eq!(get_u64(&empty, offsets::MAX_TIMESTAMP), 0);
    Segment::from_bytes(empty.clone()).expect("the canonical empty range opens");
    let mut forged_empty = empty.clone();
    set_u64(&mut forged_empty, offsets::MIN_TIMESTAMP, 0);
    assert!(
        Segment::from_bytes(forged_empty).is_err(),
        "an empty segment declaring a non-canonical range is corrupt"
    );

    cleanup(&directory);
    evidence(
        "mutation",
        &[
            "forged_min_refused_both_opens",
            "forged_low_max_refused_both_opens",
            "forged_mid_max_refused_both_opens",
            "empty_range_is_canonical",
        ],
    );
}

/// Seals a store holding exactly two spans, at record timestamps 100 and
/// 300, and returns the sealed segment's path — the two-span corpus the
/// fence-collusion tests below forge. One record per bound keeps the
/// arithmetic bare: the head window `[100, 150]` selects only the first
/// record, the tail window `[200, 400]` only the second.
fn sealed_two_span_store(label: &str) -> (PathBuf, traza::Config) {
    let directory = temp_dir(label);
    let config = traza::Config {
        durability: traza::Durability::Buffered,
        compaction: None,
        ..traza::Config::default()
    };
    let store = traza::Store::open(&directory, config.clone()).expect("store opens");
    for (span_id, start) in [("s-head", 100u64), ("s-tail", 300u64)] {
        let span: traza::Span = serde_json::from_value(serde_json::json!({
            "trace_id": "t", "span_id": span_id, "name": "op", "service": "svc",
            "start_time_ns": start, "end_time_ns": start + 50,
        }))
        .expect("span parses");
        store.ingest(span).expect("span ingests");
    }
    store.flush().expect("store flushes");
    drop(store);
    (directory, config)
}

/// The sealed segment inside a store directory.
fn sealed_segment_path(directory: &Path) -> PathBuf {
    fs::read_dir(directory)
        .expect("store directory lists")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("the flush sealed a segment")
}

fn forge_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Byte offset of block `index`'s min-timestamp fence in the directory.
fn fence_offset(bytes: &[u8], index: usize) -> usize {
    let directory_offset = get_u64(bytes, offsets::DIRECTORY_OFFSET) as usize;
    directory_offset + index * DIRECTORY_ENTRY_LEN + 24
}

/// The store-level contract every forgery test below asserts: an error at
/// open, an error at query, or the record itself — never a clean empty
/// answer.
fn assert_never_a_clean_omission(
    directory: &Path,
    config: traza::Config,
    since: u64,
    until: u64,
    span_id: &str,
) {
    match traza::Store::open(directory, config) {
        Err(refusal) => {
            let text = refusal.to_string();
            assert!(
                text.contains("corrupt"),
                "the open-time refusal names corruption: {text}"
            );
        }
        Ok(store) => {
            let filter = traza::SpanFilter {
                since_ns: Some(since),
                until_ns: Some(until),
                ..traza::SpanFilter::default()
            };
            match store.query(&filter) {
                Err(_) => {}
                Ok(spans) => assert!(
                    spans.iter().any(|span| span.span_id == span_id),
                    "a decodable record was silently omitted by the forged header range \
                     (window [{since}, {until}] answered {} spans)",
                    spans.len()
                ),
            }
        }
    }
}

/// Second-round review, the min-side residual: the header min is pinned to
/// the first block fence, but the fence is derived metadata no checksum
/// covers — so raising the header min word AND the first fence by the same
/// amount (sixteen bytes, the same class the change claimed closed) passed
/// `Segment::open`, and `may_contain_timestamps` then pruned head-window
/// queries with a clean empty answer. The open must instead decode the
/// first block, whose fence-vs-first-record check turns the collusion into
/// `Corrupt` — a forgery has to alter record bytes plus the block crc32 to
/// survive, at which point it is caught as any other corruption is.
#[test]
fn a_min_fence_collusion_cannot_silently_hide_the_head_window() {
    let (directory, config) = sealed_two_span_store("min-fence-collusion");
    let segment_path = sealed_segment_path(&directory);
    let mut bytes = fs::read(&segment_path).expect("segment reads");
    assert_eq!(get_u64(&bytes, offsets::MIN_TIMESTAMP), 100);
    // The collusion: header min 100 -> 200 AND the first fence 100 -> 200,
    // sixteen edited bytes that agree with each other.
    forge_u64(&mut bytes, offsets::MIN_TIMESTAMP, 200);
    let fence = fence_offset(&bytes, 0);
    assert_eq!(get_u64(&bytes, fence), 100, "the first fence is the min");
    forge_u64(&mut bytes, fence, 200);
    fs::write(&segment_path, &bytes).expect("tampered segment writes");

    // The head window [100, 150] holds exactly the record at 100, which the
    // forged range [200, 300] denies. Never a clean empty answer.
    assert_never_a_clean_omission(&directory, config, 100, 150, "s-head");
    cleanup(&directory);
    evidence("mutation", &["min_fence_collusion_never_a_silent_omission"]);
}

/// The symmetric tail forgery: lowering the header max alone (eight bytes,
/// no fence edit — fences carry minima, so no fence pins the max) let the
/// segment prune its own tail with a clean empty answer for as long as no
/// read happened to touch the hidden record. The open must instead read the
/// last record's timestamp — records are timestamp-sorted, so it IS the
/// true max — and require it to equal the declared max.
#[test]
fn a_lowered_header_max_cannot_silently_hide_the_tail_window() {
    let (directory, config) = sealed_two_span_store("lowered-header-max");
    let segment_path = sealed_segment_path(&directory);
    let mut bytes = fs::read(&segment_path).expect("segment reads");
    assert_eq!(get_u64(&bytes, offsets::MAX_TIMESTAMP), 300);
    // Eight bytes: header max 300 -> 150. The last fence (this segment is
    // one block, so it is also the first) still reads 100, and 150 >= 100
    // keeps the forgery consistent with every fence.
    forge_u64(&mut bytes, offsets::MAX_TIMESTAMP, 150);
    fs::write(&segment_path, &bytes).expect("tampered segment writes");

    // The tail window [200, 400] holds exactly the record at 300, which the
    // forged range [100, 150] denies. Never a clean empty answer.
    assert_never_a_clean_omission(&directory, config, 200, 400, "s-tail");
    cleanup(&directory);
    evidence("mutation", &["lowered_max_never_a_silent_omission"]);
}

/// The tail-side COLLUSION on a multi-block segment: lowering the header
/// max below the last fence is caught at open, so the colluding forgery
/// lowers the last fence with it — the same sixteen-byte shape as the
/// min-side collusion, and it passed the lazy open the same way. The
/// exact-max check at open decodes the last record's block, whose
/// fence-vs-first-record check refuses the forged fence.
#[test]
fn a_max_fence_collusion_is_refused_at_lazy_open() {
    // Two records big enough that each gets its own compression block: the
    // last fence is then the SECOND record's timestamp, 300, distinct from
    // the first fence at 100.
    let records = vec![
        RecordInput::new(
            100,
            "trace-head",
            attributes(&[("service", "checkout")]),
            vec![0xAB; COMPRESSION_BLOCK_BYTES],
        ),
        RecordInput::new(
            300,
            "trace-tail",
            attributes(&[("service", "checkout")]),
            vec![0xCD; COMPRESSION_BLOCK_BYTES],
        ),
    ];
    let honest = segment::encode(&records).expect("encode");
    let directory_len = get_u64(&honest, offsets::DIRECTORY_LEN);
    assert_eq!(
        directory_len as usize / DIRECTORY_ENTRY_LEN,
        2,
        "the corpus must span two blocks for the fences to differ"
    );
    let mut forged = honest.clone();
    assert_eq!(get_u64(&forged, offsets::MAX_TIMESTAMP), 300);
    assert_eq!(get_u64(&forged, fence_offset(&forged, 1)), 300);
    // The collusion: header max 300 -> 150 AND the last fence 300 -> 150.
    // The fences stay sorted (100 <= 150) and the declared max sits at the
    // forged fence, so every metadata-only cross-check still agrees.
    let last_fence = fence_offset(&forged, 1);
    forge_u64(&mut forged, offsets::MAX_TIMESTAMP, 150);
    forge_u64(&mut forged, last_fence, 150);

    // The eager open decodes everything and refuses.
    assert!(
        Segment::from_bytes(forged.clone()).is_err(),
        "the eager open refuses the max-side fence collusion"
    );
    // The lazy open must refuse too: its exact-max check decodes the last
    // block, and the fence-vs-first-record check catches the forged fence.
    let directory = temp_dir("max-fence-collusion");
    let path = directory.join("segment-00000000000000000000.seg");
    fs::write(&path, &forged).expect("forged segment writes");
    let refusal = Segment::open(&path).expect_err("the lazy open refuses the collusion");
    assert!(
        refusal
            .to_string()
            .contains("block min timestamp does not match its first record"),
        "the refusal names the fence-vs-record check: {refusal}"
    );

    // An INFLATED max — no record reaches it — is the other direction the
    // exact-max check closes: the last record's timestamp must EQUAL the
    // declared max, not merely stay below it.
    let mut inflated = honest.clone();
    forge_u64(&mut inflated, offsets::MAX_TIMESTAMP, 1_000);
    assert!(
        Segment::from_bytes(inflated.clone()).is_err(),
        "the eager open refuses an inflated max"
    );
    let inflated_path = directory.join("segment-00000000000000000001.seg");
    fs::write(&inflated_path, &inflated).expect("inflated segment writes");
    let refusal = Segment::open(&inflated_path).expect_err("the lazy open refuses an inflated max");
    assert!(
        refusal
            .to_string()
            .contains("header max timestamp does not match the last record"),
        "the refusal names the exact-max check: {refusal}"
    );

    cleanup(&directory);
    evidence(
        "mutation",
        &[
            "max_fence_collusion_refused_both_opens",
            "inflated_max_refused_both_opens",
        ],
    );
}
