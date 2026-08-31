//! Segment file-backed storage and persisted indexes.
//!
//! The on-disk format has exactly one version (see `MAGIC`/`VERSION`), and a
//! file declaring any other is refused rather than interpreted.
//!
//! It did not always. The format grew by appending header fields behind
//! `if version >= N` gates, each of which turned a field into an `Option` that
//! every reader downstream had to treat as "unknown, so assume the worst" —
//! a segment whose timestamp range could not be read had to be scanned. Those
//! branches were compatibility with formats that shipped in tagged 0.x
//! releases and are no longer read, so they were deleted along with the second
//! attribute-index decoder kept alive for them. What is left is one shape, and
//! the pruning path no longer carries a case where it cannot prune. Stores
//! written by those releases do not open; the README's pre-1.0 terms allow the
//! break, and `Store::open` names the file and points at an export-and-reingest
//! migration rather than at a fresh start.
//!
//! The version word stays. It is two bytes per file and it is the difference
//! between refusing to open a future format and misparsing its header into
//! plausible-looking garbage offsets — which is not a cost worth saving on the
//! one occasion it matters. It is also what triggers the one migration this
//! build performs: a file declaring v6 is converted by `Store::open` before
//! anything is served (`src/migration.rs`, where the frozen v6 decoder lives
//! and this module's refusal is never consulted). This module still reads
//! exactly one version.
//!
//! An opened file-backed segment owns its file handle, its decoded index
//! maps, and a small decoded-block cache ([`BLOCK_CACHE_SLOTS`], whose doc
//! states the bound and the worst case). In-memory segments built for
//! encoding may own their bytes. Records are decoded only when a query
//! selects their offsets; no decoded record vector is retained by
//! [`Segment`], and the block cache is counted by
//! [`Segment::resident_bytes`] rather than hidden. Loops that decode many
//! records in ascending order additionally pin their current block through a
//! request-scoped [`BlockWalk`], so their per-block decode cost never depends
//! on what concurrent readers do to the shared cache.
//!
//! # The v7 records region
//!
//! Since format 7 the records region is carved into record-aligned
//! compression blocks ([`COMPRESSION_BLOCK_BYTES`] of uncompressed bytes
//! each), stored under the codec the header names — LZ4, with raw
//! passthrough for blocks compression does not strictly shrink — and
//! addressed through a resident block directory. Records carry `(key id,
//! value digest)` pairs instead of value text; the text lives only in the
//! payload, from which the pair list is derivable (`span_to_record` in the
//! crate root is the definition). Posting lists keep their u64 currency as
//! LOGICAL offsets into the uncompressed region: the logical-to-physical
//! translation is confined to this module, and callers never see a stored
//! offset. The layout contract is docs/segment-format.md.
//!
//! # Why the attribute index is hashed
//!
//! Through format 3 — a layout this build no longer reads — the attribute
//! index was keyed on the attribute VALUE TEXT, and every opened segment held
//! every distinct value resident for its whole life.
//! For enum-shaped attributes — `service`, `status`, a model name — that is
//! nothing. For the data Traza exists to store it is fatal: an indexed
//! `gen_ai.prompt` is kilobytes, every value is distinct, and the resident
//! index therefore grew to roughly the size of the corpus text. A store that
//! reads records from disk on demand specifically so it can outgrow RAM was
//! pulling the largest part of each record back into RAM through its own
//! index.
//!
//! v4 keys the index on a 128-bit digest of the value instead (see
//! [`crate::hash`]), so a posting entry costs 20 bytes whether the value is a
//! status code or a page of text. The digest is not reversible, which makes
//! every probe a CANDIDATE list rather than an answer — see
//! [`Segment::attribute_candidate_offsets`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::content;
use crate::crc::crc32;
use crate::hash::{hash_attribute, Hash128};

/// Eight-byte marker at the beginning of every segment file. The version
/// lives in [`VERSION`], not in the magic, so the marker itself stays fixed
/// across format revisions.
pub const MAGIC: [u8; 8] = *b"TRAZASEG";
/// On-disk format version written and accepted by this module. This — not the
/// magic — is how the format generation is identified; the magic only says
/// "a Traza segment".
///
/// Exactly one value is readable. A file declaring anything else is refused,
/// which is the entire job of this constant: without it a later format's
/// header would be parsed under this one's field layout and yield offsets that
/// pass every bounds check while pointing at the wrong bytes.
///
/// **The numbering never restarts.** Versions 1 through 6 were all written by
/// real builds: 1 was JSONL, 2 shipped in 0.16/0.17 and 3 in 0.18/0.19, 4 and
/// 5 existed only on unreleased `main`, and 6 was written by every release
/// before v0.24.0. Collapsing the reader to one format does not free those
/// identifiers — reusing 2 for a different layout would mean a header
/// declaring "2" is ambiguous between two incompatible files, which is the
/// precise failure this field exists to prevent. Removing compatibility CODE
/// and reusing compatibility IDENTIFIERS are different acts, and only the
/// first is safe.
pub const VERSION: u16 = 7;
/// Fixed header size written by this module.
pub const HEADER_LEN: usize = 128;
/// Uncompressed record bytes one compression block targets. The writer cuts
/// the block before a record whose end would cross this bound, so a block
/// always holds whole records and at least one of them; a single record
/// larger than the bound becomes a block by itself. No record ever spans two
/// blocks — a reader treats one that would as corrupt.
pub const COMPRESSION_BLOCK_BYTES: usize = 128 * 1024;
/// Bytes of one block-directory entry.
pub const DIRECTORY_ENTRY_LEN: usize = 32;
/// Raw-passthrough flag in a directory entry's stored-length word: bit 31 set
/// means the block's stored bytes ARE its uncompressed bytes. The flag exists
/// so an incompressible block costs its raw size plus a directory entry and
/// never more; its position is why one record's encoding must stay below 2^31
/// bytes.
const STORED_RAW_FLAG: u32 = 1 << 31;
/// Decoded blocks kept resident per open segment, most recently used first.
/// Four covers a window search's boundary probes and a posting walk's
/// locality while bounding the cache at four blocks' uncompressed bytes.
///
/// The retained bytes are real residency and are counted by
/// [`Segment::resident_bytes`]: nominally 4 × [`COMPRESSION_BLOCK_BYTES`]
/// (512 KiB) per open segment once queries have touched it, retained for the
/// segment's life. The worst case is larger, because a block is bounded by
/// its largest RECORD, not by the carving target — a single oversized record
/// becomes a block by itself, capped only by the 2^31 record bound — so a
/// segment holding multi-megabyte spans can retain four such blocks. There is
/// no store-wide budget across segments, deliberately: the per-segment bound
/// times the open-segment count is the whole story, it is visible through
/// `Store::resident_payload_bytes`, and a shared eviction pool would couple
/// every reader through one lock for a cost ceiling the compaction size cap
/// already keeps small.
const BLOCK_CACHE_SLOTS: usize = 4;

/// Records covered by one content-index block.
///
/// This is the granularity a content query narrows to: a block whose filter
/// admits the query has all of its records decoded and checked. Smaller blocks
/// prune more precisely but make the bit-sliced matrix taller relative to the
/// data; 128 keeps the index near 1-2% of segment size while still discarding
/// 99% of a segment on a selective term.
pub const CONTENT_BLOCK_RECORDS: u32 = 128;
/// Bounds on one block filter's size. The upper bound is what stops a block of
/// pathologically varied text from sizing its own filter without limit.
const CONTENT_BLOCK_MIN_BYTES: usize = 64;
const CONTENT_BLOCK_MAX_BYTES: usize = 8 * 1024;
/// Bounds on the per-segment summary filter, which is the only part of the
/// content index held resident. The upper bound is the whole resident-memory
/// story: total cost is at most this times the number of open segments, and it
/// does not depend on how much text those segments hold.
const CONTENT_SUMMARY_MIN_BYTES: usize = 256;
const CONTENT_SUMMARY_MAX_BYTES: usize = 32 * 1024;
/// Fixed prologue of the content section, before any bitmap.
const CONTENT_PROLOGUE_LEN: usize = 32;

const RECORD_FIXED_LEN: usize = 8 + 4 + 4 + 4 + 4;
/// Bytes one encoded attribute pair occupies: key id plus value digest.
const ATTRIBUTE_PAIR_LEN: usize = 4 + 16;

/// Codec applied to the records region's compression blocks, named by the
/// header at offset 12.
///
/// **Parameterization, not a version.** An unknown id is refused with an
/// error that names it — the same shape as the version refusal, and for the
/// same reason: decoding bytes under the wrong codec produces garbage, not
/// errors. Adding a codec is a new id plus configuration, never a format
/// bump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Blocks are stored as their raw bytes. Still carved, directed, and
    /// CRC-checked identically — writing uncompressed segments is a codec
    /// choice, not a format variant, so there is exactly one reader shape.
    Raw,
    /// LZ4 block format (not the frame format), as `lz4_flex`'s block API
    /// produces it, with no length prefix of its own: the directory carries
    /// the lengths.
    Lz4,
}

impl Codec {
    /// The id this codec writes at header offset 12.
    pub fn id(self) -> u32 {
        match self {
            Self::Raw => 0,
            Self::Lz4 => 1,
        }
    }

    /// The codec's short name, for errors and verification receipts.
    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Lz4 => "lz4",
        }
    }

    fn from_id(id: u32) -> Result<Self, Error> {
        match id {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Lz4),
            other => Err(Error::UnsupportedCodec { found: other }),
        }
    }
}

/// Fixed file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Format version.
    pub version: u16,
    /// Header length in bytes.
    pub header_len: u16,
    /// Codec the records region's blocks are stored under.
    pub codec: Codec,
    /// Number of encoded records.
    pub record_count: u64,
    /// Byte offset of the record region.
    pub records_offset: u64,
    /// Length of the record region AS STORED — compressed bytes, not logical
    /// ones. [`Self::records_logical_len`] carries the uncompressed length.
    pub records_len: u64,
    /// Byte offset of the record-offset index.
    pub offsets_offset: u64,
    /// Length of the record-offset index.
    pub offsets_len: u64,
    /// Byte offset of the trace index.
    pub trace_index_offset: u64,
    /// Length of the trace index.
    pub trace_index_len: u64,
    /// Byte offset of the attribute index.
    pub attribute_index_offset: u64,
    /// Length of the attribute index.
    pub attribute_index_len: u64,
    /// Inclusive `(min, max)` record timestamp.
    ///
    /// Always present, so a query can always try to rule the segment out. An
    /// empty segment carries an empty range (`min > max`), which overlaps
    /// nothing — that is a real answer, not an unknown one.
    pub timestamps: (u64, u64),
    /// Byte offset and length of the content index.
    ///
    /// Always present too. A segment written without content indexing still
    /// carries the section: it holds a prologue declaring zero blocks, so
    /// "indexed nothing" is stated in the section rather than by its absence.
    pub content: (u64, u64),
    /// Byte offset of the block directory.
    pub directory_offset: u64,
    /// Length of the block directory: [`DIRECTORY_ENTRY_LEN`] bytes per
    /// compression block.
    pub directory_len: u64,
    /// LOGICAL length of the record region — the uncompressed byte count the
    /// posting offsets address. Carried in the header because the directory
    /// entry has no room for the last block's uncompressed size, and because
    /// record-offset validation needs the logical bound.
    pub records_logical_len: u64,
}

impl Header {
    /// Parses and validates the fixed header and all section bounds.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        Self::parse_with_total(bytes, bytes.len() as u64)
    }

    /// Parses the header from the file HEAD while validating section bounds
    /// against `total` (the real file length) — the file-backed open hands in
    /// only the head bytes.
    pub fn parse_with_total(bytes: &[u8], total: u64) -> Result<Self, Error> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Corrupt("file is shorter than the header"));
        }
        if bytes[..8] != MAGIC {
            return Err(Error::Unsupported("not a Traza segment (bad magic)"));
        }
        let version = read_u16(bytes, 8)?;
        if version != VERSION {
            // Refusing is the point. Parsing a header this module does not
            // recognize under this module's field layout would produce offsets
            // that satisfy every bounds check below while addressing the wrong
            // bytes, and the failure would surface as corrupt records rather
            // than as an unreadable file.
            return Err(Error::UnsupportedVersion {
                found: version,
                expected: VERSION,
            });
        }
        let header_len = read_u16(bytes, 10)?;
        if usize::from(header_len) != HEADER_LEN {
            return Err(Error::Corrupt("header length does not match the version"));
        }
        // The codec refusal follows the version refusal and shares its shape:
        // both name what the file declares, because both failures mean "the
        // wrong decoder", not "damaged bytes".
        let codec = Codec::from_id(read_u32(bytes, 12)?)?;
        let attribute_index_offset = read_u64(bytes, 72)?;
        // The content index follows the attribute index, so its offset is what
        // bounds the attribute section.
        let content_offset = read_u64(bytes, 96)?;
        let header = Self {
            version,
            header_len,
            codec,
            record_count: read_u64(bytes, 16)?,
            records_offset: read_u64(bytes, 24)?,
            records_len: read_u64(bytes, 32)?,
            offsets_offset: read_u64(bytes, 40)?,
            offsets_len: read_u64(bytes, 48)?,
            trace_index_offset: read_u64(bytes, 56)?,
            trace_index_len: read_u64(bytes, 64)?,
            attribute_index_offset,
            attribute_index_len: content_offset
                .checked_sub(attribute_index_offset)
                .ok_or(Error::Corrupt("attribute index offset beyond file"))?,
            timestamps: (read_u64(bytes, 80)?, read_u64(bytes, 88)?),
            content: (
                content_offset,
                total
                    .checked_sub(content_offset)
                    .ok_or(Error::Corrupt("content index offset beyond file"))?,
            ),
            directory_offset: read_u64(bytes, 104)?,
            directory_len: read_u64(bytes, 112)?,
            records_logical_len: read_u64(bytes, 120)?,
        };
        header.validate_total(total)?;
        Ok(header)
    }

    fn validate_total(&self, file_len: u64) -> Result<(), Error> {
        let sections = [
            (self.records_offset, self.records_len),
            (self.directory_offset, self.directory_len),
            (self.offsets_offset, self.offsets_len),
            (self.trace_index_offset, self.trace_index_len),
            (self.attribute_index_offset, self.attribute_index_len),
            self.content,
        ];
        let mut expected = u64::from(self.header_len);
        for (offset, len) in sections {
            if offset != expected {
                return Err(Error::Corrupt("sections are not contiguous"));
            }
            expected = offset
                .checked_add(len)
                .ok_or(Error::Corrupt("section bounds overflow"))?;
            if expected > file_len {
                return Err(Error::Corrupt("section exceeds file bounds"));
            }
        }
        if expected != file_len {
            return Err(Error::Corrupt("trailing or unaccounted segment bytes"));
        }
        let expected_offsets = self
            .record_count
            .checked_mul(8)
            .ok_or(Error::Corrupt("record-offset index length overflow"))?;
        if self.offsets_len != expected_offsets {
            return Err(Error::Corrupt("record-offset index has invalid length"));
        }
        if self.directory_len % DIRECTORY_ENTRY_LEN as u64 != 0 {
            return Err(Error::Corrupt("block directory has a partial entry"));
        }
        // Emptiness is all-or-nothing: a segment holds records exactly when it
        // holds stored bytes, logical bytes, and directory entries. Any mixed
        // state describes bytes that cannot be addressed.
        let empty = self.record_count == 0;
        if (self.records_len == 0) != empty
            || (self.records_logical_len == 0) != empty
            || (self.directory_len == 0) != empty
        {
            return Err(Error::Corrupt("record region and directory disagree"));
        }
        Ok(())
    }
}

/// A record supplied to the encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordInput {
    /// Record timestamp used for ordering and inclusive range filtering.
    pub timestamp: u64,
    /// Trace identifier indexed for exact lookup.
    pub trace_id: String,
    /// String attributes indexed as exact key/value pairs.
    pub attributes: BTreeMap<String, String>,
    /// Opaque payload bytes returned unchanged by queries.
    pub payload: Vec<u8>,
    /// Raw text this record should be findable by, for content search.
    ///
    /// Separate from `attributes` because those hold values in their canonical
    /// JSON form — a string value arrives here as `"hello\nworld"`, quotes,
    /// escapes and all. Tokenizing that would index `nworld`, and a search for
    /// `world` would silently miss. Content search needs the text as a human
    /// wrote it, so the caller supplies it unescaped.
    ///
    /// Empty means "index no content for this record". A record with no
    /// content is simply never a content-search candidate.
    pub content: Vec<String>,
}

impl RecordInput {
    /// Creates an input record with no content-search text.
    pub fn new(
        timestamp: u64,
        trace_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            timestamp,
            trace_id: trace_id.into(),
            attributes,
            payload: payload.into(),
            content: Vec::new(),
        }
    }

    /// Attaches the raw text this record should be findable by.
    pub fn with_content(mut self, content: Vec<String>) -> Self {
        self.content = content;
        self
    }
}

/// A lazily decoded record returned by a segment query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    timestamp: u64,
    trace_id: String,
    /// `(key id, value digest)` pairs in ascending key-id order. The value
    /// TEXT is not here — it lives in the payload, from which the pair list
    /// is derivable (the format's derivation invariant).
    pairs: Vec<(u32, Hash128)>,
    payload: Vec<u8>,
}

impl Record {
    /// Returns the record timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the trace identifier.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// The record's `(key id, value digest)` attribute pairs, ascending by
    /// key id. Key ids index the segment's attribute-key dictionary; value
    /// text is recoverable only from the payload.
    pub fn attribute_pairs(&self) -> &[(u32, Hash128)] {
        &self.pairs
    }

    /// Whether this record carries exactly `(key_id, digest)`. A 20-byte
    /// compare: false positives are possible under a true digest collision,
    /// false negatives are not, so the payload parse remains the authority.
    fn carries(&self, key_id: u32, digest: Hash128) -> bool {
        self.pairs
            .binary_search_by(|(id, _)| id.cmp(&key_id))
            .map(|position| self.pairs[position].1 == digest)
            .unwrap_or(false)
    }

    /// Returns the opaque payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Errors produced while encoding, opening, or querying a segment.
#[derive(Debug)]
pub enum Error {
    /// Filesystem error.
    Io(io::Error),
    /// The input is structurally invalid or truncated.
    Corrupt(&'static str),
    /// The file declares a format version this build does not read.
    ///
    /// **This says nothing about the rest of the file.** The version word is
    /// checked before any section bound is validated, so a file reported here
    /// may also be truncated or corrupt — the check establishes only that this
    /// reader is the wrong one, not that another reader will succeed.
    ///
    /// Separate from [`Self::Unsupported`] because the two call for different
    /// responses: a version mismatch has a build that can probably read it,
    /// while anything else under "unsupported" has no known reader at all.
    /// Conflating them produced advice to delete the store in response to a
    /// single flipped byte in a magic number.
    UnsupportedVersion {
        /// The version the file declares.
        found: u16,
        /// The version this build reads.
        expected: u16,
    },
    /// The file declares a records-region codec id this build does not
    /// decode. Same shape as the version refusal, for the same reason:
    /// decoding under the wrong codec produces garbage, not errors.
    UnsupportedCodec {
        /// The codec id the file declares.
        found: u32,
    },
    /// The file is not something this module can interpret at all — a foreign
    /// or corrupt magic, or an index built with incompatible parameters.
    Unsupported(&'static str),
    /// A string field is not valid UTF-8.
    Utf8(std::str::Utf8Error),
    /// A value cannot be represented by the format's fixed-width length. The
    /// string names the field, and — for the whole-record bound, the one
    /// refusal an operator may have to act on — the record itself, by trace
    /// id and timestamp.
    TooLarge(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "segment I/O error: {error}"),
            Self::Corrupt(message) => write!(f, "corrupt segment: {message}"),
            Self::UnsupportedVersion { found, expected } => write!(
                f,
                "segment format v{found}, but this build reads v{expected}"
            ),
            Self::UnsupportedCodec { found } => write!(
                f,
                "segment codec id {found}, but this build reads 0 (raw) and 1 (lz4)"
            ),
            Self::Unsupported(message) => write!(f, "unsupported segment: {message}"),
            Self::Utf8(error) => write!(f, "invalid segment UTF-8: {error}"),
            Self::TooLarge(field) => write!(f, "segment {field} is too large"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Utf8(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::Utf8(error)
    }
}

/// The resident attribute index: attribute keys held once by name, values
/// held only as digests.
///
/// Splitting keys from values this way is what bounds the structure. Attribute
/// KEYS are a schema — a store sees tens of them, they repeat on every span,
/// and an operator needs their names to understand a cost report — so they are
/// interned once in a dictionary and referred to by a `u32`. Attribute VALUES
/// are data, unbounded in both size and cardinality, so they are never
/// retained at all: only [`Hash128`] of the `(key, value)` pair survives.
///
/// The cost of one distinct value is therefore `size_of::<(u32, Hash128)>()`
/// plus 8 bytes per posting, independent of how long the value was.
#[derive(Debug, Default)]
struct AttributeIndex {
    /// Attribute key names, indexed by key id.
    keys: Vec<String>,
    /// Reverse dictionary for probe lookups.
    key_ids: HashMap<String, u32>,
    /// `(key id, value digest)` to record offsets, in record order.
    postings: HashMap<(u32, Hash128), Vec<u64>>,
}

impl AttributeIndex {
    fn key_id(&self, key: &str) -> Option<u32> {
        self.key_ids.get(key).copied()
    }

    /// Candidate offsets for a `(key, value)` probe. Empty when the segment
    /// has never seen the key, which is the common case for a filter on an
    /// attribute a given service does not emit.
    fn candidates(&self, key: &str, value: &str) -> &[u64] {
        let Some(id) = self.key_id(key) else {
            return &[];
        };
        self.postings
            .get(&(id, hash_attribute(key, value)))
            .map_or(&[], Vec::as_slice)
    }

    fn len(&self) -> usize {
        self.postings.len()
    }

    /// Approximate resident bytes, on the same "structural sum at allocated
    /// capacity" basis as [`Segment::approx_index_bytes`].
    fn approx_bytes(&self) -> usize {
        let dictionary = self.keys.iter().map(String::capacity).sum::<usize>()
            + self.key_ids.keys().map(String::capacity).sum::<usize>()
            + hash_table_bytes::<String, u32>(self.key_ids.capacity())
            + self.keys.capacity() * std::mem::size_of::<String>();
        let entries = hash_table_bytes::<(u32, Hash128), Vec<u64>>(self.postings.capacity())
            + self
                .postings
                .values()
                .map(|postings| postings.capacity() * std::mem::size_of::<u64>())
                .sum::<usize>();
        dictionary + entries
    }
}

/// The content index: a per-segment summary filter held resident, and a
/// bit-sliced matrix of per-block filters left on disk.
///
/// The layout is what makes this affordable to probe. Storing each block's
/// filter contiguously would mean reading every block's whole bitmap — several
/// hundred kilobytes per segment — to test a handful of bits in each. Stored
/// transposed, one ROW per bit position spanning all blocks, testing a bit
/// across the entire segment is a single read of `block_count` bits. A
/// two-word query touches `2 x HASH_COUNT` rows, so it reads tens of bytes per
/// segment instead of hundreds of kilobytes.
#[derive(Debug)]
struct ContentIndex {
    /// Records per block; the last block may be short.
    block_records: u32,
    block_count: u32,
    /// Bits in one block's filter, which is also the number of rows.
    block_bits: u64,
    /// Bytes per row: one bit per block, rounded up.
    row_bytes: u64,
    /// Absolute file offset of row 0.
    rows_offset: u64,
    /// The only resident part.
    summary: content::Bloom,
}

impl ContentIndex {
    /// Whether any block may hold every token — the cheap, resident test that
    /// skips a whole segment without reading a byte of it.
    fn may_contain(&self, query: &content::Query) -> bool {
        self.summary.may_contain_all(query.tokens())
    }

    /// Blocks whose filters admit every token, as a bitmap over block indexes.
    fn candidate_blocks(
        &self,
        query: &content::Query,
        backing: &Backing,
    ) -> Result<Vec<u8>, Error> {
        let row_bytes = self.row_bytes as usize;
        // Start with every block admitted, then intersect one row at a time.
        let mut admitted = vec![0xffu8; row_bytes];
        for token in query.tokens() {
            for position in content::bit_positions(token, self.block_bits as usize) {
                let offset = self
                    .rows_offset
                    .checked_add(position as u64 * self.row_bytes)
                    .ok_or(Error::Corrupt("content row offset overflow"))?;
                let row = backing.read_range(offset, self.row_bytes)?;
                for (target, source) in admitted.iter_mut().zip(row.iter()) {
                    *target &= *source;
                }
                if admitted.iter().all(|byte| *byte == 0) {
                    return Ok(admitted);
                }
            }
        }
        Ok(admitted)
    }
}

/// One block-directory entry, held resident for the life of the segment:
/// 32 bytes per 128 KiB of uncompressed records, which is the whole resident
/// price of random access into compressed ones.
#[derive(Clone, Copy, Debug)]
struct BlockEntry {
    /// Offset of the block's first byte in the UNCOMPRESSED records region.
    logical_start: u64,
    /// Offset of the block's first stored byte, relative to the records
    /// section start.
    stored_offset: u64,
    /// Stored length with the raw flag masked off.
    stored_len: u32,
    /// Bit 31 of the stored-length word: the block is stored as its raw
    /// bytes.
    raw: bool,
    /// CRC-32 over the stored bytes exactly as they appear in the file,
    /// checked on every block read before decode.
    crc32: u32,
    /// Timestamp of the block's first record. Records are timestamp-sorted,
    /// so this column is a sorted array a window probe can binary-search.
    min_timestamp: u64,
}

impl BlockEntry {
    /// The stored-length word as written: masked length plus the raw flag.
    fn stored_len_word(&self) -> u32 {
        if self.raw {
            self.stored_len | STORED_RAW_FLAG
        } else {
            self.stored_len
        }
    }
}

/// Most-recently-used decoded blocks, newest first. Shared `Arc`s so a hit
/// costs a pointer clone and eviction never invalidates a reader mid-decode.
struct BlockCache {
    slots: Mutex<Vec<(usize, Arc<Vec<u8>>)>>,
}

impl BlockCache {
    fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::with_capacity(BLOCK_CACHE_SLOTS)),
        }
    }

    fn get(&self, block: usize) -> Option<Arc<Vec<u8>>> {
        let mut slots = self.slots.lock().ok()?;
        let position = slots.iter().position(|(held, _)| *held == block)?;
        let hit = slots.remove(position);
        let bytes = hit.1.clone();
        slots.insert(0, hit);
        Some(bytes)
    }

    fn insert(&self, block: usize, bytes: Arc<Vec<u8>>) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        slots.retain(|(held, _)| *held != block);
        slots.insert(0, (block, bytes));
        slots.truncate(BLOCK_CACHE_SLOTS);
    }

    /// Decoded bytes the cache currently retains — real residency, counted
    /// by [`Segment::resident_bytes`] so the accounting surfaces cannot
    /// report a busy store's block cache as zero.
    fn resident_bytes(&self) -> usize {
        self.slots
            .lock()
            .map(|slots| slots.iter().map(|(_, bytes)| bytes.len()).sum())
            .unwrap_or(0)
    }
}

impl fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let held = self.slots.lock().map(|slots| slots.len()).unwrap_or(0);
        write!(f, "BlockCache({held} blocks)")
    }
}

/// A request-scoped memo over one segment's decoded-block cache: the last
/// block this caller decoded, pinned by its `Arc` for the walk's own reuse.
/// Created by [`Segment::walk`], and bound to that segment by borrow, so a
/// walk can never serve one segment's bytes for another's offsets.
///
/// A query walks its candidate offsets in ascending order, so consecutive
/// records overwhelmingly share a block. Without the memo every record went
/// back through the shared cache, and the shared cache is exactly that —
/// shared: a concurrent sequential scan (compaction parsing a segment it is
/// merging is the live case) can evict a block between two of a walk's
/// records, at which point the walk re-reads, re-checks, and re-inflates the
/// same block once per record. The memo bounds that to once per block PER
/// WALK, whatever the cache does; under the canonical bench it is the
/// difference between a trace lookup inflating one block and inflating up to
/// one per record.
///
/// Correctness never depends on the memo: the decoded bytes are the same
/// `Arc`-shared, CRC-checked block the cache holds. The held block is
/// transient per-request residency, exactly like a reader mid-decode, and is
/// therefore not part of [`Segment::resident_bytes`] — the shared cache
/// remains the only retained copy.
#[derive(Debug)]
pub struct BlockWalk<'a> {
    segment: &'a Segment,
    /// `(block index, decoded bytes)`.
    held: Option<(usize, Arc<Vec<u8>>)>,
}

impl BlockWalk<'_> {
    /// [`Segment::record_at_offset`] through this walk's memo, for loops
    /// that decode many offsets in ascending order: consecutive records in
    /// one block reuse the walk's decoded block instead of going back
    /// through the shared cache per record.
    pub fn record_at_offset(&mut self, relative_offset: u64) -> Result<Record, Error> {
        self.segment
            .decode_at_walked(&mut self.held, relative_offset)
    }

    /// [`Segment::record`] through this walk's memo, for sequential ordinal
    /// scans (a window parse, a rewrite pass).
    pub fn record(&mut self, ordinal: usize) -> Result<Option<Record>, Error> {
        self.segment
            .last_query_used_index
            .store(true, Ordering::Relaxed);
        match self.segment.record_offsets.get(ordinal) {
            Some(offset) => self
                .segment
                .decode_at_walked(&mut self.held, *offset)
                .map(Some),
            None => Ok(None),
        }
    }
}

/// An opened segment backed by either a file or encoded memory.
///
/// File-backed segments retain only offsets and persisted index postings.
/// Query results are decoded from their exact byte ranges on demand.
#[derive(Debug)]
pub struct Segment {
    backing: Backing,
    header: Header,
    record_offsets: Vec<u64>,
    /// The resident block directory, in block order.
    directory: Vec<BlockEntry>,
    /// Ordinal of each block's first record — derived at open by locating
    /// every block's logical start in the offset table, which doubles as the
    /// proof that no record spans a block boundary.
    block_start_ordinals: Vec<usize>,
    /// Decoded-block cache; the logical-to-physical translation's whole cost
    /// beyond the directory itself.
    cache: BlockCache,
    trace_index: HashMap<String, Vec<u64>>,
    attribute_index: AttributeIndex,
    /// `None` when the segment holds no indexable text, or was written with
    /// indexing switched off. Absent means unknown, so a content query cannot
    /// skip the segment.
    content: Option<ContentIndex>,
    /// Diagnostic only: whether the last query narrowed through an index.
    /// Atomic rather than `Cell` because a segment is shared across reader
    /// threads — pinned views and the segment list both hand out `&Segment`.
    last_query_used_index: AtomicBool,
}

/// Where a segment's payload bytes live.
///
/// `Resident` holds the full encoding in memory (the state right after
/// `build`/`from_bytes`). `File` holds only the opened file plus the parsed
/// indexes — record payloads are read on demand, exactly the byte range each
/// access needs, which is what makes stores larger than RAM serveable.
enum Backing {
    Resident(Vec<u8>),
    File {
        file: std::sync::Mutex<fs::File>,
        len: u64,
    },
}

impl fmt::Debug for Backing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resident(bytes) => write!(f, "Resident({} bytes)", bytes.len()),
            Self::File { len, .. } => write!(f, "File({len} bytes)"),
        }
    }
}

impl Backing {
    fn read_range(&self, start: u64, len: u64) -> Result<Vec<u8>, Error> {
        let start_usize = start as usize;
        let len_usize = len as usize;
        match self {
            Self::Resident(bytes) => bytes
                .get(start_usize..start_usize.saturating_add(len_usize))
                .map(<[u8]>::to_vec)
                .ok_or(Error::Corrupt("range outside segment bytes")),
            Self::File { file, len: total } => {
                if start.saturating_add(len) > *total {
                    return Err(Error::Corrupt("range outside segment file"));
                }
                use std::io::{Read, Seek, SeekFrom};
                let mut guard = file
                    .lock()
                    .map_err(|_| Error::Corrupt("file lock poisoned"))?;
                guard.seek(SeekFrom::Start(start))?;
                let mut buffer = vec![0u8; len_usize];
                guard.read_exact(&mut buffer)?;
                Ok(buffer)
            }
        }
    }

    fn total_len(&self) -> u64 {
        match self {
            Self::Resident(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
        }
    }

    fn resident_len(&self) -> usize {
        match self {
            Self::Resident(bytes) => bytes.len(),
            Self::File { .. } => 0,
        }
    }
}

impl Segment {
    /// Opens and validates an encoded segment from owned bytes. Validation is
    /// EAGER here: every block is read, CRC-checked, and decoded, and every
    /// record decoded from it — the byte-resident counterpart of the lazy
    /// file-backed open.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, Error> {
        let header = Header::parse(&bytes)?;
        let record_offsets = decode_offsets(&bytes, &header)?;
        validate_record_offsets_lengths(&header, &record_offsets)?;
        let directory = decode_directory(
            section(&bytes, header.directory_offset, header.directory_len)?,
            &header,
        )?;
        let block_start_ordinals = block_start_ordinals(&header, &record_offsets, &directory)?;
        let trace_index = decode_string_index(
            section(&bytes, header.trace_index_offset, header.trace_index_len)?,
            false,
            header.record_count,
        )?
        .into_iter()
        .map(|((key, _), offsets)| (key, offsets))
        .collect();
        let attribute_index = decode_attribute_index(
            section(
                &bytes,
                header.attribute_index_offset,
                header.attribute_index_len,
            )?,
            header.record_count,
        )?;
        let (content_offset, content_len) = header.content;
        let content = decode_content_head(
            section(&bytes, content_offset, content_len)?,
            content_offset,
            content_len,
            header.record_count,
        )?;
        let segment = Self {
            backing: Backing::Resident(bytes),
            header,
            record_offsets,
            directory,
            block_start_ordinals,
            cache: BlockCache::new(),
            trace_index,
            attribute_index,
            content,
            last_query_used_index: AtomicBool::new(false),
        };
        let mut held = None;
        for offset in &segment.record_offsets {
            segment.decode_at_walked(&mut held, *offset)?;
        }
        Ok(segment)
    }

    /// Opens a segment FILE-BACKED: only the header and index sections are
    /// read into memory; record payloads stay on disk and are fetched on
    /// demand. This is the larger-than-RAM path.
    ///
    /// "Only the indexes" is not the same as "a bounded amount". The index
    /// sections are decoded EAGERLY and in full, so this call's resident cost
    /// scales with the segment's index CARDINALITY: roughly 20 bytes per
    /// distinct `(attribute key, value)` pair plus 8 bytes per posting, plus
    /// the trace index, which is still keyed on trace-id text.
    ///
    /// What it no longer scales with is the SIZE of attribute values. Through
    /// format 3 it did, and an indexed prompt cost its own text; the attribute
    /// index is keyed on a digest now. [`Self::approx_index_bytes`] measures
    /// the result.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = fs::File::open(path)?;
        let total = file.metadata()?.len();
        // Parse the header from the file head; Header::parse needs the total
        // length for its trailing-length arithmetic, so hand it a buffer whose
        // length IS the file length but only materialize the head + indexes.
        let head_len = (HEADER_LEN as u64).min(total) as usize;
        let mut head = vec![0u8; head_len];
        file.read_exact(&mut head)?;
        let header = Header::parse_with_total(&head, total)?;
        let mut read_section = |offset: u64, len: u64| -> Result<Vec<u8>, Error> {
            if offset.saturating_add(len) > total {
                return Err(Error::Corrupt("index section outside file"));
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut buffer = vec![0u8; len as usize];
            file.read_exact(&mut buffer)?;
            Ok(buffer)
        };
        let offsets_bytes = read_section(header.offsets_offset, header.offsets_len)?;
        let record_offsets = decode_offsets_from(&offsets_bytes, &header)?;
        validate_record_offsets_lengths(&header, &record_offsets)?;
        let directory_bytes = read_section(header.directory_offset, header.directory_len)?;
        let directory = decode_directory(&directory_bytes, &header)?;
        let block_start_ordinals = block_start_ordinals(&header, &record_offsets, &directory)?;
        let trace_bytes = read_section(header.trace_index_offset, header.trace_index_len)?;
        let trace_index = decode_string_index(&trace_bytes, false, header.record_count)?
            .into_iter()
            .map(|((key, _), offsets)| (key, offsets))
            .collect();
        let attribute_bytes =
            read_section(header.attribute_index_offset, header.attribute_index_len)?;
        let attribute_index = decode_attribute_index(&attribute_bytes, header.record_count)?;
        // Only the prologue and the summary filter are read: the bit-sliced
        // block rows stay on disk and are fetched a row at a time by a query.
        let content = {
            let (offset, len) = header.content;
            let prologue_len = (CONTENT_PROLOGUE_LEN as u64).min(len);
            let prologue = read_section(offset, prologue_len)?;
            let summary_bits = if prologue.len() >= CONTENT_PROLOGUE_LEN {
                read_u64(&prologue, 16)?
            } else {
                0
            };
            let head_len = (CONTENT_PROLOGUE_LEN as u64 + summary_bits / 8).min(len);
            let head = read_section(offset, head_len)?;
            decode_content_head(&head, offset, len, header.record_count)?
        };
        Ok(Self {
            backing: Backing::File {
                file: Mutex::new(file),
                len: total,
            },
            header,
            record_offsets,
            directory,
            block_start_ordinals,
            cache: BlockCache::new(),
            trace_index,
            attribute_index,
            content,
            last_query_used_index: AtomicBool::new(false),
        })
    }

    /// Bytes of record/payload content currently resident in memory: the
    /// whole file for `from_bytes` segments, zero for freshly opened
    /// file-backed ones — plus whatever decoded blocks the block cache
    /// retains once queries have touched the segment (at most
    /// [`BLOCK_CACHE_SLOTS`] blocks, see that constant for the bound). The
    /// larger-than-RAM rule is stated against this number: zero at open and
    /// at flush, cache-bounded afterwards.
    pub fn resident_bytes(&self) -> usize {
        self.backing.resident_len() + self.cache.resident_bytes()
    }

    /// **Approximate** bytes held resident by this segment's decoded indexes.
    ///
    /// [`Self::resident_bytes`] answers "how much of the *file* is in
    /// memory", which is zero for a file-backed segment. That is the number
    /// the larger-than-RAM rule is stated in, and on its own it is
    /// misleading: the indexes are decoded eagerly by [`Self::open`] and stay
    /// resident for the life of the segment. This counts them.
    ///
    /// It is approximate, and it has to be: exact allocator accounting needs
    /// either a dependency or `unsafe`, and this crate permits neither. What
    /// is counted is the structural sum — every `String` and `Vec` the
    /// indexes own, at its allocated capacity, plus each hash table's bucket
    /// array. What is NOT counted is the allocator's per-allocation
    /// bookkeeping and size-class rounding, which apply once per string and
    /// once per posting list, nor the segment struct itself. **Read it as a
    /// floor.** Process RSS runs above it.
    pub fn approx_index_bytes(&self) -> usize {
        let offsets = self.record_offsets.capacity() * std::mem::size_of::<u64>();
        let directory = self.directory.capacity() * std::mem::size_of::<BlockEntry>()
            + self.block_start_ordinals.capacity() * std::mem::size_of::<usize>();
        let traces = hash_table_bytes::<String, Vec<u64>>(self.trace_index.capacity())
            + self
                .trace_index
                .iter()
                .map(|(key, postings)| {
                    key.capacity() + postings.capacity() * std::mem::size_of::<u64>()
                })
                .sum::<usize>();
        offsets + directory + traces + self.attribute_index.approx_bytes()
    }

    /// Distinct `(key, value)` pairs in the resident attribute index — the
    /// cardinality that [`Self::approx_index_bytes`] is driven by.
    pub fn attribute_index_len(&self) -> usize {
        self.attribute_index.len()
    }

    /// Resident attribute-index cost split by attribute KEY: distinct values
    /// and approximate bytes, on the same basis as
    /// [`Self::approx_index_bytes`] minus the shared table array.
    ///
    /// A total cannot answer the only question an operator actually has when
    /// the number is too big — *which attribute is doing this* — and the
    /// answer is rarely uniform. Since v4 the answer is far flatter than it
    /// used to be: an entry costs the same whether its value was `"error"` or
    /// a page of generated text, so what shows up here now is genuine
    /// CARDINALITY rather than text volume.
    pub fn attribute_index_cost_by_key(&self) -> BTreeMap<String, (usize, usize)> {
        let mut by_key: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for ((key_id, _), postings) in &self.attribute_index.postings {
            let Some(key) = self.attribute_index.keys.get(*key_id as usize) else {
                continue;
            };
            let entry = by_key.entry(key.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += std::mem::size_of::<(u32, Hash128)>()
                + postings.capacity() * std::mem::size_of::<u64>();
        }
        by_key
    }

    /// Encodes records and constructs a byte-resident segment.
    pub fn build(records: &[RecordInput]) -> Result<Self, Error> {
        Self::from_bytes(encode(records)?)
    }

    /// Returns the validated file header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Returns the complete encoded segment bytes (reads the whole file for
    /// file-backed segments — an inspection/persist path, not a query path).
    pub fn encoded_bytes(&self) -> Result<Vec<u8>, Error> {
        self.backing.read_range(0, self.backing.total_len())
    }

    /// Returns the number of records without decoding them.
    pub fn len(&self) -> usize {
        self.record_offsets.len()
    }

    /// Returns whether the segment contains no records.
    pub fn is_empty(&self) -> bool {
        self.record_offsets.is_empty()
    }

    /// Returns the number of decoded records retained by this segment.
    ///
    /// V2 segments never retain decoded records, so this is always zero.
    pub fn resident_decoded_record_count(&self) -> usize {
        0
    }

    /// Returns whether the most recent query selected candidates using an index.
    pub fn last_query_used_index(&self) -> bool {
        self.last_query_used_index.load(Ordering::Relaxed)
    }

    /// Decodes one record by ordinal through the persisted offset table.
    pub fn record(&self, ordinal: usize) -> Result<Option<Record>, Error> {
        self.walk().record(ordinal)
    }

    /// A fresh [`BlockWalk`] over this segment — the entry point for any
    /// loop that decodes offsets or ordinals in ascending order.
    pub fn walk(&self) -> BlockWalk<'_> {
        BlockWalk {
            segment: self,
            held: None,
        }
    }

    /// Looks up records for an exact trace identifier.
    pub fn query_trace(&self, trace_id: &str) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.store(true, Ordering::Relaxed);
        self.decode_postings(self.trace_index.get(trace_id))
    }

    /// Looks up records for an exact attribute key/value pair.
    ///
    /// The index is probed by digest, so it answers with candidates; each one
    /// is then checked against the `(key id, digest)` pair the RECORD itself
    /// carries, which is what keeps a forged index posting from decoding into
    /// a wrong row: the wasted work is one block decode, never a false match
    /// from the index alone.
    ///
    /// The check is a digest compare, not a value compare — v7 records carry
    /// no value text, and the payload is opaque at this layer. A true 128-bit
    /// collision (two distinct values, one digest) therefore passes here by
    /// construction, exactly as it passes the index probe; the store's
    /// verification against the parsed payload (`span_matches` and its
    /// relatives in `lib.rs`) remains the authority on every result it
    /// returns. An index narrows a filter; it never answers one.
    pub fn query_attribute(&self, key: &str, value: &str) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.store(true, Ordering::Relaxed);
        let mut records = Vec::new();
        let Some(key_id) = self.attribute_index.key_id(key) else {
            return Ok(records);
        };
        let digest = hash_attribute(key, value);
        let mut held = None;
        for offset in self.attribute_index.candidates(key, value) {
            let record = self.decode_at_walked(&mut held, *offset)?;
            if record.carries(key_id, digest) {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.timestamp);
        Ok(records)
    }

    /// Whether `record` carries the digest pair for `(key, value)` — the
    /// cheap prefilter a caller runs before parsing the record's payload, so
    /// the parse is paid only on digest matches. False positives are possible
    /// under a true digest collision, false negatives are not; the caller's
    /// payload-derived comparison stays the authority.
    pub fn record_carries_attribute(&self, record: &Record, key: &str, value: &str) -> bool {
        self.attribute_index
            .key_id(key)
            .is_some_and(|key_id| record.carries(key_id, hash_attribute(key, value)))
    }

    /// Returns records in the inclusive timestamp range in stable timestamp order.
    pub fn query_time_range(&self, start: u64, end: u64) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.store(true, Ordering::Relaxed);
        if start > end {
            return Ok(Vec::new());
        }
        let span = self.ordinal_range_for_window(Some(start), Some(end))?;
        let mut records = Vec::with_capacity(span.len());
        let mut held = None;
        for ordinal in span {
            let offset = self.record_offsets[ordinal];
            records.push(self.decode_at_walked(&mut held, offset)?);
        }
        Ok(records)
    }

    /// The half-open ordinal range `[start, end)` of every record whose
    /// timestamp falls in the inclusive window, found by binary search.
    ///
    /// This is the whole reason `encode_with` sorts: records are stored in
    /// ascending timestamp order, so a window is a contiguous ordinal range
    /// and locating it never decodes the segment. The directory's sorted
    /// min-timestamp column narrows each bound to ONE candidate block with no
    /// I/O at all, and the per-record search then runs inside that block —
    /// at most a boundary-block decode per bound, cached across its probes.
    /// Each landed bound is then VERIFIED against the records on both sides,
    /// because the fence column is derived metadata no checksum covers: a
    /// corrupt-but-sorted fence answers `Corrupt`, never a shifted window.
    ///
    /// `None` means unbounded on that side. The returned range is empty when
    /// the window selects nothing.
    pub fn ordinal_range_for_window(
        &self,
        since: Option<u64>,
        until: Option<u64>,
    ) -> Result<std::ops::Range<usize>, Error> {
        let count = self.record_offsets.len();
        let first_at_or_after = |bound: u64| -> Result<usize, Error> {
            // The first block whose min timestamp reaches the bound: every
            // record before the PREVIOUS block's end is below the bound, and
            // every record from this block on is at or above it, so only that
            // previous block needs its records examined.
            let next = self
                .directory
                .partition_point(|entry| entry.min_timestamp < bound);
            let low = match next.checked_sub(1) {
                Some(candidate) => {
                    let (mut low, mut high) = (
                        self.block_start_ordinals[candidate],
                        self.block_start_ordinals
                            .get(candidate + 1)
                            .copied()
                            .unwrap_or(count),
                    );
                    // `partition_point`, but each probe can fail: the
                    // timestamp is read through the block decode for a
                    // file-backed segment.
                    while low < high {
                        let mid = low + (high - low) / 2;
                        if self.timestamp_at(self.record_offsets[mid])? < bound {
                            low = mid + 1;
                        } else {
                            high = mid;
                        }
                    }
                    low
                }
                None => 0,
            };
            // The fence steered the search; the RECORDS confirm the answer.
            // The min-timestamp column is derived metadata no checksum
            // covers, and a corrupt-but-still-sorted fence would otherwise
            // shift a window bound silently — returning out-of-window rows
            // or dropping in-window ones without ever decoding the forged
            // block. Verifying the boundary against the records themselves
            // costs at most two timestamp reads (usually in the block the
            // search just decoded) and turns every such forgery into
            // `Corrupt`, on an honest segment it can never fire. The
            // matching per-block check lives in `decoded_block`, which
            // compares each decoded block's first record with its fence.
            if low < count && self.timestamp_at(self.record_offsets[low])? < bound {
                return Err(Error::Corrupt(
                    "block min timestamps disagree with the records",
                ));
            }
            if low > 0 && self.timestamp_at(self.record_offsets[low - 1])? >= bound {
                return Err(Error::Corrupt(
                    "block min timestamps disagree with the records",
                ));
            }
            Ok(low)
        };
        let start = match since {
            Some(bound) => first_at_or_after(bound)?,
            None => 0,
        };
        // The window is inclusive at the top, so the end is the first record
        // strictly after it — `until + 1`, saturating so that `u64::MAX` as an
        // upper bound selects the tail rather than wrapping to nothing.
        let end = match until {
            Some(bound) => match bound.checked_add(1) {
                Some(exclusive) => first_at_or_after(exclusive)?,
                None => count,
            },
            None => count,
        };
        Ok(start..end.max(start))
    }

    /// Writes the exact encoded bytes to a file and synchronizes them.
    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        use std::io::Write;
        let mut file = fs::File::create(path)?;
        let encoded = self.encoded_bytes()?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    }

    /// Inclusive `(min, max)` record timestamp.
    pub fn timestamp_range(&self) -> (u64, u64) {
        self.header().timestamps
    }

    /// Whether any record here can fall inside `[since, until]`.
    ///
    /// This is the whole point of the range: a query that answers `false` skips
    /// the segment without reading a byte of it. There is no "unknown" case —
    /// there used to be, for segments predating the field, and it meant every
    /// time-bounded query scanned them in full.
    pub fn may_contain_timestamps(&self, since: Option<u64>, until: Option<u64>) -> bool {
        let (min, max) = self.timestamp_range();
        if min > max {
            // Empty segment; nothing can match.
            return false;
        }
        if since.is_some_and(|since| max < since) {
            return false;
        }
        if until.is_some_and(|until| min > until) {
            return false;
        }
        true
    }

    /// All record offsets in record (timestamp) order — the lazy full-scan
    /// candidate list when no index applies.
    pub fn record_offsets(&self) -> &[u64] {
        &self.record_offsets
    }

    /// CANDIDATE record offsets for one attribute key/value pair, in record
    /// (timestamp) order — no records are decoded. The lazy query path pairs
    /// this with [`Self::timestamp_at`] and [`Self::record_at_offset`] so a
    /// limited query decodes only the records it returns.
    ///
    /// # Candidates, not matches
    ///
    /// The index is keyed on a 128-bit digest of the value, so this list is a
    /// superset: a caller MUST check each decoded record against the filter
    /// before returning it. Every caller in this crate already does, because
    /// an index has never been allowed to change a filter's answer — only to
    /// narrow the work it takes to compute one. The name says "candidate" so
    /// that a future caller cannot mistake it for a resolved answer.
    ///
    /// The superset is tiny. A collision needs two distinct values in one
    /// segment whose digests agree in all 128 bits; at a million distinct
    /// values per segment the odds are about one in 10^26.
    pub fn attribute_candidate_offsets(&self, key: &str, value: &str) -> &[u64] {
        self.last_query_used_index.store(true, Ordering::Relaxed);
        self.attribute_index.candidates(key, value)
    }

    /// Whether this segment may hold a record matching `query`.
    ///
    /// Answered from the resident summary filter alone — no I/O — so a store
    /// with hundreds of segments discards most of them for the cost of a few
    /// hash lookups each. `true` on a segment with no content index, because
    /// absent means unknown.
    pub fn may_contain_content(&self, query: &content::Query) -> bool {
        match &self.content {
            Some(index) if query.is_indexable() => index.may_contain(query),
            _ => true,
        }
    }

    /// CANDIDATE record offsets for a content query, or `None` when the index
    /// cannot narrow it and every record is a candidate.
    ///
    /// Reads one bit-sliced row per (token, hash) pair — tens of bytes for a
    /// typical query — and returns the records of every block those rows
    /// admit. Like every index in this crate the result is a superset: a Bloom
    /// filter has false positives, and blocks are 128 records wide, so most
    /// candidates will not match. [`content::Query::matches`] against the
    /// decoded span is what decides.
    pub fn content_candidate_offsets(
        &self,
        query: &content::Query,
    ) -> Result<Option<Vec<u64>>, Error> {
        let Some(index) = &self.content else {
            return Ok(None);
        };
        if !query.is_indexable() {
            return Ok(None);
        }
        if !index.may_contain(query) {
            self.last_query_used_index.store(true, Ordering::Relaxed);
            return Ok(Some(Vec::new()));
        }
        let admitted = index.candidate_blocks(query, &self.backing)?;
        self.last_query_used_index.store(true, Ordering::Relaxed);
        let block_records = index.block_records as usize;
        let mut offsets = Vec::new();
        for block in 0..index.block_count as usize {
            if admitted[block / 8] & (1 << (block % 8)) == 0 {
                continue;
            }
            let start = block * block_records;
            let end = (start + block_records).min(self.record_offsets.len());
            offsets.extend_from_slice(&self.record_offsets[start..end]);
        }
        Ok(Some(offsets))
    }

    /// Whether this segment carries a content index at all.
    pub fn has_content_index(&self) -> bool {
        self.content.is_some()
    }

    /// Fraction of the resident summary filter's bits that are set, or `None`
    /// without a content index.
    ///
    /// A ratio approaching 1.0 means the filter has saturated: it still cannot
    /// return a wrong answer, but it has stopped skipping segments and content
    /// search has degraded to a scan. This is the number that says so.
    pub fn content_summary_fill(&self) -> Option<f64> {
        self.content
            .as_ref()
            .map(|index| index.summary.fill_ratio())
    }

    /// Resident bytes held by the content index — the summary filter only.
    /// The bit-sliced block rows stay on disk.
    pub fn content_resident_bytes(&self) -> usize {
        self.content
            .as_ref()
            .map_or(0, |index| index.summary.as_bytes().len())
    }

    /// Timestamp of the record at a posting offset without decoding the
    /// record: the timestamp is its first fixed field, read out of the
    /// containing block's decoded bytes (one block decode, usually cached).
    pub fn timestamp_at(&self, relative_offset: u64) -> Result<u64, Error> {
        if relative_offset >= self.header.records_logical_len {
            return Err(Error::Corrupt("record offset is outside record region"));
        }
        let block = self.block_containing(relative_offset)?;
        let entry = self.directory[block];
        let end = relative_offset
            .checked_add(8)
            .ok_or(Error::Corrupt("record offset overflow"))?;
        if end > self.block_logical_end(block) {
            return Err(Error::Corrupt("record timestamp crosses block bounds"));
        }
        let bytes = self.decoded_block(block)?;
        read_u64(&bytes, (relative_offset - entry.logical_start) as usize)
    }

    /// Decodes exactly one record at a posting offset.
    pub fn record_at_offset(&self, relative_offset: u64) -> Result<Record, Error> {
        self.decode_at(relative_offset)
    }

    fn decode_postings(&self, postings: Option<&Vec<u64>>) -> Result<Vec<Record>, Error> {
        let mut records = Vec::new();
        if let Some(postings) = postings {
            let mut walk = self.walk();
            for offset in postings {
                records.push(walk.record_at_offset(*offset)?);
            }
        }
        records.sort_by_key(|record| record.timestamp);
        Ok(records)
    }

    /// Index of the block containing logical offset `logical`, by binary
    /// search over the resident directory. The translation from posting
    /// offsets to stored bytes lives entirely here and in
    /// [`Self::decoded_block`]; every caller above deals in logical offsets.
    fn block_containing(&self, logical: u64) -> Result<usize, Error> {
        self.directory
            .partition_point(|entry| entry.logical_start <= logical)
            .checked_sub(1)
            .ok_or(Error::Corrupt("offset precedes the first block"))
    }

    /// The logical end of `block`: the next block's start, or the region's
    /// logical length for the last block.
    fn block_logical_end(&self, block: usize) -> u64 {
        self.directory
            .get(block + 1)
            .map_or(self.header.records_logical_len, |entry| entry.logical_start)
    }

    /// [`Self::decoded_block`] through a walk's memo. The memo answers
    /// first; on a miss the shared cache and the decode path answer exactly
    /// as before, and the memo pins what they returned.
    fn decoded_block_walked(
        &self,
        held: &mut Option<(usize, Arc<Vec<u8>>)>,
        block: usize,
    ) -> Result<Arc<Vec<u8>>, Error> {
        if let Some((held_block, bytes)) = held {
            if *held_block == block {
                return Ok(bytes.clone());
            }
        }
        let bytes = self.decoded_block(block)?;
        *held = Some((block, bytes.clone()));
        Ok(bytes)
    }

    /// The decoded bytes of one block. The stored bytes are read under the
    /// backing's lock (inside `read_range`); the CRC check and the inflation
    /// run OUTSIDE it, so a slow decode never blocks other readers' I/O.
    fn decoded_block(&self, block: usize) -> Result<Arc<Vec<u8>>, Error> {
        if let Some(bytes) = self.cache.get(block) {
            return Ok(bytes);
        }
        let entry = *self
            .directory
            .get(block)
            .ok_or(Error::Corrupt("block index out of range"))?;
        let absolute = self
            .header
            .records_offset
            .checked_add(entry.stored_offset)
            .ok_or(Error::Corrupt("block offset overflow"))?;
        let stored = self
            .backing
            .read_range(absolute, u64::from(entry.stored_len))?;
        // Checked before decode, always: a flipped stored byte must surface
        // as this error and never as a decoder failure or garbage records.
        if crc32(&stored) != entry.crc32 {
            return Err(Error::Corrupt("block crc32 mismatch"));
        }
        let logical_len = (self.block_logical_end(block) - entry.logical_start) as usize;
        let decoded = if entry.raw {
            stored
        } else {
            match self.header.codec {
                Codec::Raw => stored,
                Codec::Lz4 => lz4_flex::block::decompress(&stored, logical_len)
                    .map_err(|_| Error::Corrupt("block does not decompress"))?,
            }
        };
        if decoded.len() != logical_len {
            return Err(Error::Corrupt("block decodes to the wrong length"));
        }
        // The directory's min-timestamp fence, verified against the block's
        // own first record (the block starts at a record boundary, and a
        // record's first field is its timestamp). The fence steers
        // `ordinal_range_for_window` without any decode, so a forged fence
        // that stays sorted would otherwise move a window bound silently —
        // and a misplaced bound either selects this block as the search
        // candidate or puts the first wrongly included record inside it, so
        // every path that could serve a wrong row decodes this block and
        // trips this check first. `from_bytes` decodes every block eagerly,
        // making the check an open-time one for byte-resident segments.
        if read_u64(&decoded, 0)? != entry.min_timestamp {
            return Err(Error::Corrupt(
                "block min timestamp does not match its first record",
            ));
        }
        let decoded = Arc::new(decoded);
        self.cache.insert(block, decoded.clone());
        Ok(decoded)
    }

    fn decode_at(&self, relative_offset: u64) -> Result<Record, Error> {
        self.decode_at_walked(&mut None, relative_offset)
    }

    fn decode_at_walked(
        &self,
        held: &mut Option<(usize, Arc<Vec<u8>>)>,
        relative_offset: u64,
    ) -> Result<Record, Error> {
        if relative_offset >= self.header.records_logical_len {
            return Err(Error::Corrupt("record offset is outside record region"));
        }
        // Exact record length from the consecutive-offsets invariant, stated
        // in logical bytes exactly as v6 stated it in stored ones.
        let position = self
            .record_offsets
            .binary_search(&relative_offset)
            .map_err(|_| Error::Corrupt("offset is not a record boundary"))?;
        let record_len = self
            .record_offsets
            .get(position + 1)
            .copied()
            .unwrap_or(self.header.records_logical_len)
            .checked_sub(relative_offset)
            .ok_or(Error::Corrupt("record length underflow"))?;
        let block = self.block_containing(relative_offset)?;
        let entry = self.directory[block];
        let end = relative_offset
            .checked_add(record_len)
            .ok_or(Error::Corrupt("record length overflow"))?;
        // One block decode suffices for any record, by construction of the
        // writer; a record that would span two blocks is corrupt.
        if end > self.block_logical_end(block) {
            return Err(Error::Corrupt("record spans compression blocks"));
        }
        let bytes = self.decoded_block_walked(held, block)?;
        let start = (relative_offset - entry.logical_start) as usize;
        decode_record(&bytes, start, start as u64 + record_len)
    }
}

/// Encodes records into a complete segment byte stream, with a content index.
pub fn encode(records: &[RecordInput]) -> Result<Vec<u8>, Error> {
    encode_with(records, true)
}

/// Encodes records, optionally omitting the content index, under the default
/// codec (LZ4).
///
/// A segment without one is still searchable — it is scanned rather than
/// skipped — so this trades query latency for seal-time CPU and about 1-2% of
/// segment size. See `Config::content_index`.
pub fn encode_with(records: &[RecordInput], content_index: bool) -> Result<Vec<u8>, Error> {
    encode_with_codec(records, content_index, Codec::Lz4)
}

/// Encodes records under an explicit codec. Codec choice changes only how
/// blocks are stored: the logical record bytes, the carving, the directory,
/// and every index section are byte-identical across codecs.
pub fn encode_with_codec(
    records: &[RecordInput],
    content_index: bool,
    codec: Codec,
) -> Result<Vec<u8>, Error> {
    // Ascending timestamp order is a FORMAT INVARIANT, not a hope about what
    // callers pass. `Segment::ordinal_range_for_window` binary-searches the
    // record region on it, and a binary search over unordered data does not
    // fail loudly — it silently answers with the wrong records. The store's
    // own writers (seal, compaction) already sort, so for them this is a
    // no-op; enforcing it here means no future caller can quietly break the
    // search. Sorting rather than rejecting keeps a mis-sorted caller from
    // turning into a failed seal that strands acknowledged spans in the
    // buffer. The sort is STABLE and by timestamp alone, so a caller's finer
    // tie-break (Traza sorts on end time, then trace, then span) survives it
    // and already-sorted input encodes to byte-identical output.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|index| records[*index].timestamp);
    let records: Vec<&RecordInput> = order.into_iter().map(|index| &records[index]).collect();
    let records = records.as_slice();

    let mut record_region = Vec::new();
    let mut offsets = Vec::with_capacity(records.len());
    let mut trace_index: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();

    // Assign key ids from the sorted set of distinct keys, so the dictionary
    // depends on WHICH keys appear and never on the order records arrived in.
    // Two encodings of the same records must produce identical bytes.
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

    for record in records {
        let offset = record_region.len() as u64;
        offsets.push(offset);
        // Ascending key-id order falls out of the BTreeMap's sorted-key
        // iteration meeting a dictionary assigned from the same sort.
        let pairs: Vec<(u32, Hash128)> = record
            .attributes
            .iter()
            .map(|(key, value)| {
                (
                    *key_ids
                        .get(key.as_str())
                        .expect("every attribute key is in the dictionary"),
                    hash_attribute(key, value),
                )
            })
            .collect();
        encode_record(&mut record_region, record, &pairs)?;
        // The raw-passthrough flag lives in the directory's stored-length
        // word, so a block — and therefore any single record — must encode
        // below 2^31 bytes. Nothing plausible comes near it; the point is
        // that an implausible record fails loudly instead of wrapping — and
        // names itself, because the spec requires the refusal to identify
        // the record, and trace id plus timestamp are in hand right here.
        if record_region.len() as u64 - offset >= u64::from(STORED_RAW_FLAG) {
            return Err(Error::TooLarge(format!(
                "record encoding (trace {:?}, timestamp {})",
                record.trace_id, record.timestamp
            )));
        }
        trace_index
            .entry((record.trace_id.clone(), String::new()))
            .or_default()
            .push(offset);
        for (id, digest) in &pairs {
            attribute_index
                .entry((*id, *digest))
                .or_default()
                .push(offset);
        }
    }

    let (stored_region, directory_region) = carve_blocks(&record_region, &offsets, records, codec);

    let mut offset_region = Vec::with_capacity(offsets.len() * 8);
    for offset in &offsets {
        put_u64(&mut offset_region, *offset);
    }
    let trace_region = encode_string_index(&trace_index, false)?;
    let attribute_region = encode_attribute_index(&attribute_keys, &attribute_index)?;
    let content_region = if content_index {
        encode_content_index(records)
    } else {
        encode_content_index(&[])
    };

    let records_offset = HEADER_LEN as u64;
    let directory_offset = records_offset + stored_region.len() as u64;
    let offsets_offset = directory_offset + directory_region.len() as u64;
    let trace_index_offset = offsets_offset + offset_region.len() as u64;
    let attribute_index_offset = trace_index_offset + trace_region.len() as u64;
    let content_index_offset = attribute_index_offset + attribute_region.len() as u64;

    let mut bytes = Vec::with_capacity(
        HEADER_LEN
            + stored_region.len()
            + directory_region.len()
            + offset_region.len()
            + trace_region.len()
            + attribute_region.len()
            + content_region.len(),
    );
    bytes.extend_from_slice(&MAGIC);
    put_u16(&mut bytes, VERSION);
    put_u16(&mut bytes, HEADER_LEN as u16);
    put_u32(&mut bytes, codec.id());
    put_u64(&mut bytes, records.len() as u64);
    put_u64(&mut bytes, records_offset);
    put_u64(&mut bytes, stored_region.len() as u64);
    put_u64(&mut bytes, offsets_offset);
    put_u64(&mut bytes, offset_region.len() as u64);
    put_u64(&mut bytes, trace_index_offset);
    put_u64(&mut bytes, trace_region.len() as u64);
    put_u64(&mut bytes, attribute_index_offset);
    // Inclusive timestamp range. An empty segment gets an empty range
    // (min > max), which can never overlap a query and is therefore always
    // skippable — the correct answer for a segment holding nothing.
    let (min_ts, max_ts) = records.iter().fold((u64::MAX, 0_u64), |(lo, hi), record| {
        (lo.min(record.timestamp), hi.max(record.timestamp))
    });
    put_u64(&mut bytes, min_ts);
    put_u64(&mut bytes, max_ts);
    put_u64(&mut bytes, content_index_offset);
    put_u64(&mut bytes, directory_offset);
    put_u64(&mut bytes, directory_region.len() as u64);
    put_u64(&mut bytes, record_region.len() as u64);
    debug_assert_eq!(bytes.len(), HEADER_LEN);
    bytes.extend_from_slice(&stored_region);
    bytes.extend_from_slice(&directory_region);
    bytes.extend_from_slice(&offset_region);
    bytes.extend_from_slice(&trace_region);
    bytes.extend_from_slice(&attribute_region);
    bytes.extend_from_slice(&content_region);
    Ok(bytes)
}

/// Carves the logical record region into compression blocks and returns the
/// stored region plus its directory, both ready to write.
///
/// A block is cut before the record whose end would cross
/// [`COMPRESSION_BLOCK_BYTES`] from the block's start, so blocks hold whole
/// records and at least one each; a single oversized record becomes a block
/// by itself. Compressed output that is not strictly smaller than its input
/// is stored raw and flagged, which bounds the worst case at raw size plus
/// the directory. Under [`Codec::Raw`] every block takes that path.
fn carve_blocks(
    record_region: &[u8],
    offsets: &[u64],
    records: &[&RecordInput],
    codec: Codec,
) -> (Vec<u8>, Vec<u8>) {
    let mut stored_region = Vec::new();
    let mut directory = Vec::new();
    let record_end = |index: usize| -> u64 {
        offsets
            .get(index + 1)
            .copied()
            .unwrap_or(record_region.len() as u64)
    };
    let mut start = 0usize;
    while start < offsets.len() {
        let logical_start = offsets[start];
        let mut end = start + 1;
        while end < offsets.len()
            && record_end(end) - logical_start <= COMPRESSION_BLOCK_BYTES as u64
        {
            end += 1;
        }
        let raw_bytes = &record_region[logical_start as usize..record_end(end - 1) as usize];
        let (stored, raw) = match codec {
            Codec::Raw => (raw_bytes.to_vec(), true),
            Codec::Lz4 => {
                let compressed = lz4_flex::block::compress(raw_bytes);
                if compressed.len() < raw_bytes.len() {
                    (compressed, false)
                } else {
                    (raw_bytes.to_vec(), true)
                }
            }
        };
        let entry = BlockEntry {
            logical_start,
            stored_offset: stored_region.len() as u64,
            // Below 2^31 by the per-record bound: a multi-record block is at
            // most COMPRESSION_BLOCK_BYTES, and a single-record block was
            // checked at encode.
            stored_len: stored.len() as u32,
            raw,
            crc32: crc32(&stored),
            min_timestamp: records[start].timestamp,
        };
        put_u64(&mut directory, entry.logical_start);
        put_u64(&mut directory, entry.stored_offset);
        put_u32(&mut directory, entry.stored_len_word());
        put_u32(&mut directory, entry.crc32);
        put_u64(&mut directory, entry.min_timestamp);
        stored_region.extend_from_slice(&stored);
        start = end;
    }
    (stored_region, directory)
}

/// Builds the content index over `records`.
///
/// Records are grouped into blocks of [`CONTENT_BLOCK_RECORDS`]. Each block
/// gets a Bloom filter over the distinct tokens of its records' text, and
/// those filters are written TRANSPOSED — one row per bit position, one bit
/// per block — so that a query can test a token against every block in the
/// segment with a single small read. See [`ContentIndex`].
///
/// A segment whose records carry no content text writes the prologue with zero
/// blocks, which the reader treats as "no content index" rather than as "no
/// content": an absent index can never be used to skip anything.
fn encode_content_index(records: &[&RecordInput]) -> Vec<u8> {
    let block_records = CONTENT_BLOCK_RECORDS as usize;
    let block_count = records.len().div_ceil(block_records);
    let indexable = records.iter().any(|record| !record.content.is_empty());
    if block_count == 0 || !indexable {
        let mut out = Vec::with_capacity(CONTENT_PROLOGUE_LEN);
        put_u32(&mut out, 0);
        put_u32(&mut out, CONTENT_BLOCK_RECORDS);
        put_u32(&mut out, 0);
        put_u32(&mut out, content::HASH_COUNT);
        put_u64(&mut out, 0);
        put_u64(&mut out, 0);
        debug_assert_eq!(out.len(), CONTENT_PROLOGUE_LEN);
        return out;
    }

    // Per-block token sets, plus the segment-wide set for the summary.
    let mut block_tokens: Vec<std::collections::HashSet<String>> = Vec::with_capacity(block_count);
    let mut all_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();
    for chunk in records.chunks(block_records) {
        let tokens = content::distinct_probe_keys(
            chunk
                .iter()
                .flat_map(|record| record.content.iter().map(String::as_str)),
        );
        for token in &tokens {
            if !all_tokens.contains(token.as_str()) {
                all_tokens.insert(token.clone());
            }
        }
        block_tokens.push(tokens);
    }

    // One filter size for the whole segment, taken from its busiest block, so
    // that a row is a fixed stride and needs no offset table.
    let widest = block_tokens.iter().map(HashSet::len).max().unwrap_or(0);
    let block_bits = content::size_bits(widest, CONTENT_BLOCK_MIN_BYTES, CONTENT_BLOCK_MAX_BYTES);
    let row_bytes = block_count.div_ceil(8);

    let mut summary = content::Bloom::new(content::size_bits(
        all_tokens.len(),
        CONTENT_SUMMARY_MIN_BYTES,
        CONTENT_SUMMARY_MAX_BYTES,
    ));
    for token in &all_tokens {
        summary.insert(token);
    }

    let mut rows = vec![0u8; block_bits * row_bytes];
    for (block, tokens) in block_tokens.iter().enumerate() {
        for token in tokens {
            for position in content::bit_positions(token, block_bits) {
                rows[position * row_bytes + block / 8] |= 1 << (block % 8);
            }
        }
    }

    let mut out = Vec::with_capacity(CONTENT_PROLOGUE_LEN + summary.as_bytes().len() + rows.len());
    put_u32(&mut out, 0);
    put_u32(&mut out, CONTENT_BLOCK_RECORDS);
    put_u32(&mut out, block_count as u32);
    put_u32(&mut out, content::HASH_COUNT);
    put_u64(&mut out, summary.bit_len() as u64);
    put_u64(&mut out, block_bits as u64);
    debug_assert_eq!(out.len(), CONTENT_PROLOGUE_LEN);
    out.extend_from_slice(summary.as_bytes());
    out.extend_from_slice(&rows);
    out
}

/// Parses the content section's prologue and resident summary filter from the
/// FRONT of the section, without needing the bit-sliced rows that follow.
///
/// `head` must hold at least the prologue and the summary; `section_len` is
/// the section's full length, against which the declared sizes are checked.
/// Splitting it this way is what lets a file-backed open read a few kilobytes
/// instead of the whole index.
fn decode_content_head(
    head: &[u8],
    section_offset: u64,
    section_len: u64,
    record_count: u64,
) -> Result<Option<ContentIndex>, Error> {
    let data = head;
    if data.len() < CONTENT_PROLOGUE_LEN {
        return Err(Error::Corrupt("content index is shorter than its prologue"));
    }
    let block_records = read_u32(data, 4)?;
    let block_count = read_u32(data, 8)?;
    let hash_count = read_u32(data, 12)?;
    let summary_bits = read_u64(data, 16)?;
    let block_bits = read_u64(data, 24)?;

    if block_count == 0 {
        // Written by a segment with no indexable text, or with content
        // indexing switched off. "Absent" must read as unknown, never as
        // empty: a filter that skipped every segment would turn content
        // search into a query that silently returns nothing.
        return Ok(None);
    }
    if hash_count != content::HASH_COUNT {
        return Err(Error::Unsupported(
            "content index was built with a different hash count",
        ));
    }
    if block_records == 0 {
        return Err(Error::Corrupt("content index has a zero block size"));
    }
    let expected_blocks = record_count.div_ceil(u64::from(block_records));
    if u64::from(block_count) != expected_blocks {
        return Err(Error::Corrupt("content index block count does not match"));
    }
    if !summary_bits.is_power_of_two() || !block_bits.is_power_of_two() {
        return Err(Error::Corrupt("content filter size is not a power of two"));
    }
    let row_bytes = u64::from(block_count).div_ceil(8);
    let summary_bytes = summary_bits / 8;
    let expected_len = CONTENT_PROLOGUE_LEN as u64 + summary_bytes + block_bits * row_bytes;
    if section_len != expected_len {
        return Err(Error::Corrupt(
            "content index length does not match its header",
        ));
    }
    let summary_start = CONTENT_PROLOGUE_LEN;
    let summary_end = summary_start + summary_bytes as usize;
    if data.len() < summary_end {
        return Err(Error::Corrupt("content index summary is truncated"));
    }
    Ok(Some(ContentIndex {
        block_records,
        block_count,
        block_bits,
        row_bytes,
        rows_offset: section_offset + summary_end as u64,
        summary: content::Bloom::from_bytes(data[summary_start..summary_end].to_vec()),
    }))
}

/// Encodes records and atomically replaces a destination file where supported.
pub fn write(path: impl AsRef<Path>, records: &[RecordInput]) -> Result<(), Error> {
    use std::io::Write;
    let path = path.as_ref();
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);
    let bytes = encode(records)?;
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn encode_record(
    output: &mut Vec<u8>,
    record: &RecordInput,
    pairs: &[(u32, Hash128)],
) -> Result<(), Error> {
    let trace_len =
        u32::try_from(record.trace_id.len()).map_err(|_| Error::TooLarge("trace id".to_owned()))?;
    let attribute_count =
        u32::try_from(pairs.len()).map_err(|_| Error::TooLarge("attribute count".to_owned()))?;
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| Error::TooLarge("payload".to_owned()))?;
    put_u64(output, record.timestamp);
    put_u32(output, trace_len);
    put_u32(output, attribute_count);
    put_u32(output, payload_len);
    put_u32(output, 0);
    output.extend_from_slice(record.trace_id.as_bytes());
    for (id, digest) in pairs {
        put_u32(output, *id);
        output.extend_from_slice(digest.as_bytes());
    }
    output.extend_from_slice(&record.payload);
    Ok(())
}

fn decode_record(bytes: &[u8], start: usize, region_end: u64) -> Result<Record, Error> {
    let region_end = usize::try_from(region_end)
        .map_err(|_| Error::Corrupt("record region does not fit memory"))?;
    if start
        .checked_add(RECORD_FIXED_LEN)
        .filter(|end| *end <= region_end)
        .is_none()
    {
        return Err(Error::Corrupt("truncated record header"));
    }
    let timestamp = read_u64(bytes, start)?;
    let trace_len = read_u32(bytes, start + 8)? as usize;
    let attribute_count = read_u32(bytes, start + 12)? as usize;
    let payload_len = read_u32(bytes, start + 16)? as usize;
    let mut cursor = start + RECORD_FIXED_LEN;
    let trace = take(bytes, &mut cursor, trace_len, region_end)?;
    let trace_id = std::str::from_utf8(trace)?.to_owned();
    // The declared count is bounded against the bytes that remain before it
    // sizes an allocation — a corrupt count must fail, not reserve gigabytes.
    if attribute_count
        .checked_mul(ATTRIBUTE_PAIR_LEN)
        .filter(|needed| {
            cursor
                .checked_add(*needed)
                .is_some_and(|end| end <= region_end)
        })
        .is_none()
    {
        return Err(Error::Corrupt("truncated record attribute pairs"));
    }
    let mut pairs: Vec<(u32, Hash128)> = Vec::with_capacity(attribute_count);
    for _ in 0..attribute_count {
        let id = take_u32_bounded(bytes, &mut cursor, region_end)?;
        let digest = take(bytes, &mut cursor, 16, region_end)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(digest);
        // Ascending key-id order is part of the encoding, and the candidate
        // prefilter binary-searches on it.
        if pairs.last().is_some_and(|(previous, _)| *previous >= id) {
            return Err(Error::Corrupt("record attribute pairs are unordered"));
        }
        pairs.push((id, Hash128::from_bytes(raw)));
    }
    let payload = take(bytes, &mut cursor, payload_len, region_end)?.to_vec();
    Ok(Record {
        timestamp,
        trace_id,
        pairs,
        payload,
    })
}

/// Decodes and validates the block directory. Every check is mandatory: a
/// directory failing any of them cannot address the bytes it claims to.
fn decode_directory(data: &[u8], header: &Header) -> Result<Vec<BlockEntry>, Error> {
    if data.len() as u64 != header.directory_len {
        return Err(Error::Corrupt("block directory length mismatch"));
    }
    let count = data.len() / DIRECTORY_ENTRY_LEN;
    let mut entries: Vec<BlockEntry> = Vec::with_capacity(count);
    let mut stored_total = 0u64;
    for index in 0..count {
        let base = index * DIRECTORY_ENTRY_LEN;
        let word = read_u32(data, base + 16)?;
        let entry = BlockEntry {
            logical_start: read_u64(data, base)?,
            stored_offset: read_u64(data, base + 8)?,
            stored_len: word & !STORED_RAW_FLAG,
            raw: word & STORED_RAW_FLAG != 0,
            crc32: read_u32(data, base + 20)?,
            min_timestamp: read_u64(data, base + 24)?,
        };
        if entry.stored_len == 0 {
            return Err(Error::Corrupt("block directory entry has zero length"));
        }
        if entry.logical_start >= header.records_logical_len {
            return Err(Error::Corrupt("block starts beyond the logical region"));
        }
        // Stored blocks are contiguous from zero: the offset IS the running
        // sum, which is a stronger claim than "strictly increasing" and is
        // what makes the length sum check below airtight.
        if entry.stored_offset != stored_total {
            return Err(Error::Corrupt("block directory stored offsets have gaps"));
        }
        stored_total = stored_total
            .checked_add(u64::from(entry.stored_len))
            .ok_or(Error::Corrupt("block directory length overflow"))?;
        if let Some(previous) = entries.last() {
            if entry.logical_start <= previous.logical_start {
                return Err(Error::Corrupt("block logical starts are unordered"));
            }
            if entry.min_timestamp < previous.min_timestamp {
                return Err(Error::Corrupt("block min timestamps are unordered"));
            }
        } else if entry.logical_start != 0 {
            return Err(Error::Corrupt("first block does not start at zero"));
        }
        entries.push(entry);
    }
    if stored_total != header.records_len {
        return Err(Error::Corrupt(
            "block directory does not account for the stored region",
        ));
    }
    // A raw block's stored bytes ARE its logical bytes, so the two lengths
    // must agree entry by entry.
    for (index, entry) in entries.iter().enumerate() {
        let logical_end = entries
            .get(index + 1)
            .map_or(header.records_logical_len, |next| next.logical_start);
        if entry.raw && u64::from(entry.stored_len) != logical_end - entry.logical_start {
            return Err(Error::Corrupt("raw block length mismatch"));
        }
    }
    Ok(entries)
}

/// Ordinal of each block's first record. Locating every block's logical start
/// in the offset table doubles as the record-alignment proof: a directory
/// whose block starts inside a record describes a record spanning two blocks,
/// which the format forbids.
///
/// It also bounds every block's LOGICAL extent to what the format can legally
/// produce, because that extent later sizes the decompress allocation — and
/// an allocation the machine cannot satisfy aborts the process instead of
/// erroring. The header's `records_logical_len` (which fixes the last block's
/// extent) carries no checksum, so without these bounds one flipped high bit
/// passed every other open-time check and crashed the store at first read.
/// The bounds are the writer's own: a block holding more than one record
/// spans at most [`COMPRESSION_BLOCK_BYTES`] (the writer cuts before the
/// record that would cross it), a single-record block stays below 2^31 (the
/// record bound the raw flag imposes), and a compressed block cannot inflate
/// past LZ4's ~255x expansion ceiling of its stored bytes. A directory
/// claiming more describes bytes no encoder wrote: `Corrupt`, never a
/// gigantic `vec![0; …]`.
fn block_start_ordinals(
    header: &Header,
    offsets: &[u64],
    directory: &[BlockEntry],
) -> Result<Vec<usize>, Error> {
    let ordinals: Vec<usize> = directory
        .iter()
        .map(|entry| {
            offsets
                .binary_search(&entry.logical_start)
                .map_err(|_| Error::Corrupt("block does not start at a record boundary"))
        })
        .collect::<Result<_, _>>()?;
    for (index, entry) in directory.iter().enumerate() {
        let logical_end = directory
            .get(index + 1)
            .map_or(header.records_logical_len, |next| next.logical_start);
        // Non-negative by decode_directory's ordering checks; the last
        // block's start was checked below `records_logical_len`.
        let extent = logical_end - entry.logical_start;
        let records_in_block = ordinals
            .get(index + 1)
            .copied()
            .unwrap_or(offsets.len())
            .saturating_sub(ordinals[index]);
        let legal = if records_in_block > 1 {
            extent <= COMPRESSION_BLOCK_BYTES as u64
        } else {
            extent < u64::from(STORED_RAW_FLAG)
        };
        if !legal {
            return Err(Error::Corrupt(
                "block logical extent exceeds the format bound",
            ));
        }
        if !entry.raw && extent > u64::from(entry.stored_len) * 256 + 64 {
            return Err(Error::Corrupt(
                "block logical extent exceeds what lz4 can decode from its stored bytes",
            ));
        }
    }
    Ok(ordinals)
}

/// Decodes the offsets index from ITS OWN section bytes (file-backed open).
fn decode_offsets_from(data: &[u8], header: &Header) -> Result<Vec<u64>, Error> {
    if data.len() as u64 != header.offsets_len {
        return Err(Error::Corrupt("offsets section length mismatch"));
    }
    let mut offsets = Vec::with_capacity(header.record_count as usize);
    for chunk in data.chunks_exact(8) {
        offsets.push(u64::from_le_bytes(
            chunk.try_into().expect("eight-byte chunk"),
        ));
    }
    Ok(offsets)
}

/// Structural offset validation without touching record bytes: ordering and
/// LOGICAL bounds only. The file-backed open validates records lazily on
/// access — a corrupt record surfaces as Error::Corrupt from that read —
/// while `from_bytes` follows this with an eager decode of every record.
fn validate_record_offsets_lengths(header: &Header, offsets: &[u64]) -> Result<(), Error> {
    let mut previous = None;
    for offset in offsets {
        if *offset >= header.records_logical_len || previous.is_some_and(|value| *offset <= value) {
            return Err(Error::Corrupt("record offsets are invalid or unordered"));
        }
        previous = Some(*offset);
    }
    if offsets.is_empty() && header.records_logical_len != 0 {
        return Err(Error::Corrupt("record region exists without records"));
    }
    Ok(())
}

fn decode_offsets(bytes: &[u8], header: &Header) -> Result<Vec<u64>, Error> {
    let data = section(bytes, header.offsets_offset, header.offsets_len)?;
    let mut offsets = Vec::with_capacity(header.record_count as usize);
    for chunk in data.chunks_exact(8) {
        offsets.push(u64::from_le_bytes(
            chunk.try_into().expect("eight-byte chunk"),
        ));
    }
    Ok(offsets)
}

/// Encodes the attribute section: a key dictionary, then one entry per
/// distinct `(key, value)` pair carrying the value's digest instead of its
/// text.
///
/// Entries are written in `(key id, digest)` order so that encoding the same
/// records twice produces the same bytes — segment files are compared byte for
/// byte by the format acceptance tests, and a merge must be reproducible.
fn encode_attribute_index(
    keys: &[String],
    postings: &BTreeMap<(u32, Hash128), Vec<u64>>,
) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    put_u32(
        &mut output,
        u32::try_from(keys.len())
            .map_err(|_| Error::TooLarge("attribute key dictionary".to_owned()))?,
    );
    for key in keys {
        put_len_bytes(&mut output, key.as_bytes(), "index key")?;
    }
    put_u32(
        &mut output,
        u32::try_from(postings.len()).map_err(|_| Error::TooLarge("index".to_owned()))?,
    );
    for ((key_id, digest), offsets) in postings {
        put_u32(&mut output, *key_id);
        output.extend_from_slice(digest.as_bytes());
        put_u32(
            &mut output,
            u32::try_from(offsets.len()).map_err(|_| Error::TooLarge("postings".to_owned()))?,
        );
        for offset in offsets {
            put_u64(&mut output, *offset);
        }
    }
    Ok(output)
}

/// Decodes the attribute section.
fn decode_attribute_index(data: &[u8], record_count: u64) -> Result<AttributeIndex, Error> {
    let mut cursor = 0usize;
    let key_count = take_u32(data, &mut cursor)? as usize;
    let mut keys = Vec::with_capacity(key_count);
    let mut key_ids = HashMap::with_capacity(key_count);
    for id in 0..key_count {
        let key = std::str::from_utf8(take_len_bytes(data, &mut cursor, data.len())?)?.to_owned();
        if key_ids.insert(key.clone(), id as u32).is_some() {
            return Err(Error::Corrupt("attribute key dictionary has a duplicate"));
        }
        keys.push(key);
    }
    let entry_count = take_u32(data, &mut cursor)? as usize;
    let mut postings = HashMap::with_capacity(entry_count);
    for _ in 0..entry_count {
        let key_id = take_u32(data, &mut cursor)?;
        if key_id as usize >= keys.len() {
            return Err(Error::Corrupt("attribute entry names an unknown key"));
        }
        let digest = take_digest(data, &mut cursor)?;
        let posting_count = take_u32(data, &mut cursor)? as usize;
        if posting_count as u64 > record_count {
            return Err(Error::Corrupt("index has too many postings"));
        }
        let mut offsets = Vec::with_capacity(posting_count);
        for _ in 0..posting_count {
            offsets.push(take_u64(data, &mut cursor)?);
        }
        if postings.insert((key_id, digest), offsets).is_some() {
            return Err(Error::Corrupt("index contains a duplicate key"));
        }
    }
    if cursor != data.len() {
        return Err(Error::Corrupt("index contains trailing bytes"));
    }
    Ok(AttributeIndex {
        keys,
        key_ids,
        postings,
    })
}

fn take_digest(data: &[u8], cursor: &mut usize) -> Result<Hash128, Error> {
    let end = cursor
        .checked_add(16)
        .filter(|end| *end <= data.len())
        .ok_or(Error::Corrupt("truncated attribute digest"))?;
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&data[*cursor..end]);
    *cursor = end;
    Ok(Hash128::from_bytes(bytes))
}

fn encode_string_index(
    index: &BTreeMap<(String, String), Vec<u64>>,
    include_value: bool,
) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    put_u32(
        &mut output,
        u32::try_from(index.len()).map_err(|_| Error::TooLarge("index".to_owned()))?,
    );
    for ((key, value), postings) in index {
        put_len_bytes(&mut output, key.as_bytes(), "index key")?;
        if include_value {
            put_len_bytes(&mut output, value.as_bytes(), "index value")?;
        }
        put_u32(
            &mut output,
            u32::try_from(postings.len()).map_err(|_| Error::TooLarge("postings".to_owned()))?,
        );
        for offset in postings {
            put_u64(&mut output, *offset);
        }
    }
    Ok(output)
}

fn decode_string_index(
    data: &[u8],
    include_value: bool,
    record_count: u64,
) -> Result<HashMap<(String, String), Vec<u64>>, Error> {
    let mut cursor = 0usize;
    let count = take_u32(data, &mut cursor)? as usize;
    let mut index = HashMap::with_capacity(count);
    for _ in 0..count {
        let key = std::str::from_utf8(take_len_bytes(data, &mut cursor, data.len())?)?.to_owned();
        let value = if include_value {
            std::str::from_utf8(take_len_bytes(data, &mut cursor, data.len())?)?.to_owned()
        } else {
            String::new()
        };
        let posting_count = take_u32(data, &mut cursor)? as usize;
        if posting_count as u64 > record_count {
            return Err(Error::Corrupt("index has too many postings"));
        }
        let mut postings = Vec::with_capacity(posting_count);
        for _ in 0..posting_count {
            postings.push(take_u64(data, &mut cursor)?);
        }
        if index.insert((key, value), postings).is_some() {
            return Err(Error::Corrupt("index contains a duplicate key"));
        }
    }
    if cursor != data.len() {
        return Err(Error::Corrupt("index contains trailing bytes"));
    }
    Ok(index)
}

/// Bytes a `std` hash table's bucket array occupies for a given
/// [`HashMap::capacity`], to within the group padding.
///
/// `capacity()` reports how many entries fit before a resize, not the bucket
/// count, and the two differ by hashbrown's 7/8 load factor. Inverting that
/// is what makes the estimate track a large index rather than undercount it
/// by an eighth: below eight entries the table is a fixed 4 or 8 buckets,
/// above it `capacity` is exactly `buckets / 8 * 7`. Each bucket costs one
/// `(K, V)` slot plus one control byte; the trailing group-width control
/// padding (8 or 16 bytes) is ignored as noise.
fn hash_table_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = if capacity < 4 {
        4
    } else if capacity < 8 {
        8
    } else {
        (capacity / 7 * 8).next_power_of_two()
    };
    buckets * (std::mem::size_of::<(K, V)>() + 1)
}

fn section(bytes: &[u8], offset: u64, len: u64) -> Result<&[u8], Error> {
    let start = usize::try_from(offset)
        .map_err(|_| Error::Corrupt("section offset does not fit memory"))?;
    let length =
        usize::try_from(len).map_err(|_| Error::Corrupt("section length does not fit memory"))?;
    let end = start
        .checked_add(length)
        .ok_or(Error::Corrupt("section bounds overflow"))?;
    bytes
        .get(start..end)
        .ok_or(Error::Corrupt("section exceeds file bounds"))
}

fn put_len_bytes(output: &mut Vec<u8>, bytes: &[u8], field: &'static str) -> Result<(), Error> {
    put_u32(
        output,
        u32::try_from(bytes.len()).map_err(|_| Error::TooLarge(field.to_owned()))?,
    );
    output.extend_from_slice(bytes);
    Ok(())
}

fn take_len_bytes<'a>(bytes: &'a [u8], cursor: &mut usize, end: usize) -> Result<&'a [u8], Error> {
    let len = take_u32_bounded(bytes, cursor, end)? as usize;
    take(bytes, cursor, len, end)
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    end: usize,
) -> Result<&'a [u8], Error> {
    let next = cursor
        .checked_add(len)
        .ok_or(Error::Corrupt("field length overflow"))?;
    if next > end || next > bytes.len() {
        return Err(Error::Corrupt("truncated variable-length field"));
    }
    let value = &bytes[*cursor..next];
    *cursor = next;
    Ok(value)
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, Error> {
    take_u32_bounded(bytes, cursor, bytes.len())
}

fn take_u32_bounded(bytes: &[u8], cursor: &mut usize, end: usize) -> Result<u32, Error> {
    let value = read_u32_bounded(bytes, *cursor, end)?;
    *cursor += 4;
    Ok(value)
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, Error> {
    let value = read_u64(bytes, *cursor)?;
    *cursor += 8;
    Ok(value)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(Error::Corrupt("truncated integer"))?;
    Ok(u16::from_le_bytes(raw.try_into().expect("two-byte slice")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    read_u32_bounded(bytes, offset, bytes.len())
}

fn read_u32_bounded(bytes: &[u8], offset: usize, end: usize) -> Result<u32, Error> {
    let next = offset
        .checked_add(4)
        .ok_or(Error::Corrupt("integer offset overflow"))?;
    if next > end {
        return Err(Error::Corrupt("truncated integer"));
    }
    let raw = bytes
        .get(offset..next)
        .ok_or(Error::Corrupt("truncated integer"))?;
    Ok(u32::from_le_bytes(raw.try_into().expect("four-byte slice")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or(Error::Corrupt("truncated integer"))?;
    Ok(u64::from_le_bytes(
        raw.try_into().expect("eight-byte slice"),
    ))
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
