//! Segment file-backed storage and persisted indexes.
//!
//! The on-disk format is version 4 (see `MAGIC`/`VERSION`); versions 2 and 3
//! are still read, and version 1 (JSONL) is not.
//!
//! An opened file-backed segment owns only its file handle and decoded index
//! maps. In-memory segments built for encoding may own their bytes. Records are
//! decoded only when a query selects their offsets; no decoded record vector
//! is retained by [`Segment`].
//!
//! # Why the attribute index is hashed
//!
//! Through v3 the attribute index was keyed on the attribute VALUE TEXT, and
//! every opened segment held every distinct value resident for its whole life.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::hash::{hash_attribute, Hash128};

/// Eight-byte marker at the beginning of every segment file. The version
/// lives in [`VERSION`], not in the magic, so the marker itself stays fixed
/// across format revisions.
pub const MAGIC: [u8; 8] = *b"TRAZASEG";
/// On-disk format version written by this module. This — not the magic — is
/// how the format generation is identified; the magic only says "a Traza
/// segment". (JSONL v1 carried no magic; this indexed format is version 2.)
pub const VERSION: u16 = 4;
/// Fixed header size written by this module.
///
/// v3 appends the segment's timestamp range to the v2 header. Time is the
/// most common filter an observability store sees and it was the one thing a
/// query could not use to skip work: `since`/`until` were pure post-filters,
/// so a "last 15 minutes" search opened and scanned every segment in the
/// store. Two u64s in the header let a query eliminate a whole segment
/// without touching it.
///
/// v4 changes only the attribute index's own encoding — see
/// [`AttributeIndex`] — so the header keeps its v3 shape and length.
pub const HEADER_LEN: usize = 96;
/// v2 header size. Still readable: a v2 segment simply carries no timestamp
/// range, and a query treats its range as unknown and cannot skip it — which
/// is exactly the behaviour that existed before v3.
pub const HEADER_LEN_V2: usize = 80;
/// Oldest format this module can read.
pub const MIN_READABLE_VERSION: u16 = 2;

const RECORD_FIXED_LEN: usize = 8 + 4 + 4 + 4 + 4;

/// Fixed v2 file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Format version.
    pub version: u16,
    /// Header length in bytes.
    pub header_len: u16,
    /// Number of encoded records.
    pub record_count: u64,
    /// Byte offset of the record region.
    pub records_offset: u64,
    /// Length of the record region.
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
    /// Inclusive `(min, max)` record timestamp, or `None` on a v2 segment
    /// that predates the field. `None` means unknown, never "empty": a query
    /// must scan a segment whose range it cannot rule out.
    pub timestamps: Option<(u64, u64)>,
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
        if bytes.len() < HEADER_LEN_V2 {
            return Err(Error::Corrupt("file is shorter than the smallest header"));
        }
        if bytes[..8] != MAGIC {
            return Err(Error::Unsupported("not a Traza segment (bad magic)"));
        }
        let version = read_u16(bytes, 8)?;
        if !(MIN_READABLE_VERSION..=VERSION).contains(&version) {
            return Err(Error::Unsupported("unsupported segment version"));
        }
        let header_len = read_u16(bytes, 10)?;
        let expected_header_len = match version {
            2 => HEADER_LEN_V2,
            _ => HEADER_LEN,
        };
        if usize::from(header_len) != expected_header_len {
            return Err(Error::Corrupt("header length does not match the version"));
        }
        if bytes.len() < expected_header_len {
            return Err(Error::Corrupt("file is shorter than its declared header"));
        }
        let header = Self {
            version,
            header_len,
            record_count: read_u64(bytes, 16)?,
            records_offset: read_u64(bytes, 24)?,
            records_len: read_u64(bytes, 32)?,
            offsets_offset: read_u64(bytes, 40)?,
            offsets_len: read_u64(bytes, 48)?,
            trace_index_offset: read_u64(bytes, 56)?,
            trace_index_len: read_u64(bytes, 64)?,
            attribute_index_offset: read_u64(bytes, 72)?,
            attribute_index_len: total
                .checked_sub(read_u64(bytes, 72)?)
                .ok_or(Error::Corrupt("attribute index offset beyond file"))?,
            // v2 predates the range. `None` means "unknown", which makes a
            // query scan the segment rather than wrongly skip it.
            timestamps: if version >= 3 {
                Some((read_u64(bytes, 80)?, read_u64(bytes, 88)?))
            } else {
                None
            },
        };
        header.validate_total(total)?;
        Ok(header)
    }

    fn validate_total(&self, file_len: u64) -> Result<(), Error> {
        let sections = [
            (self.records_offset, self.records_len),
            (self.offsets_offset, self.offsets_len),
            (self.trace_index_offset, self.trace_index_len),
            (self.attribute_index_offset, self.attribute_index_len),
        ];
        let mut expected = u64::from(self.header_len);
        for (offset, len) in sections {
            if offset != expected {
                return Err(Error::Corrupt("v2 sections are not contiguous"));
            }
            expected = offset
                .checked_add(len)
                .ok_or(Error::Corrupt("v2 section bounds overflow"))?;
            if expected > file_len {
                return Err(Error::Corrupt("v2 section exceeds file bounds"));
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
        Ok(())
    }
}

/// A record supplied to the v2 encoder.
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
}

impl RecordInput {
    /// Creates an input record.
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
        }
    }
}

/// A lazily decoded record returned by a segment query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    timestamp: u64,
    trace_id: String,
    attributes: BTreeMap<String, String>,
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

    /// Returns the decoded attributes.
    pub fn attributes(&self) -> &BTreeMap<String, String> {
        &self.attributes
    }

    /// Returns the opaque payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Errors produced while encoding, opening, or querying a v2 segment.
#[derive(Debug)]
pub enum Error {
    /// Filesystem error.
    Io(io::Error),
    /// The input is structurally invalid or truncated.
    Corrupt(&'static str),
    /// The file uses an unsupported magic or version.
    Unsupported(&'static str),
    /// A string field is not valid UTF-8.
    Utf8(std::str::Utf8Error),
    /// A value cannot be represented by the format's fixed-width length.
    TooLarge(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "segment I/O error: {error}"),
            Self::Corrupt(message) => write!(f, "corrupt v2 segment: {message}"),
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

/// An opened segment backed by either a file or encoded memory.
///
/// File-backed segments retain only offsets and persisted index postings.
/// Query results are decoded from their exact byte ranges on demand.
#[derive(Debug)]
pub struct Segment {
    backing: Backing,
    header: Header,
    record_offsets: Vec<u64>,
    trace_index: HashMap<String, Vec<u64>>,
    attribute_index: AttributeIndex,
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
    /// Opens and validates an encoded segment from owned bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, Error> {
        let header = Header::parse(&bytes)?;
        let record_offsets = decode_offsets(&bytes, &header)?;
        validate_record_offsets(&bytes, &header, &record_offsets)?;
        let trace_index = decode_string_index(
            section(&bytes, header.trace_index_offset, header.trace_index_len)?,
            false,
            header.record_count,
        )?
        .into_iter()
        .map(|((key, _), offsets)| (key, offsets))
        .collect();
        let attribute_index = read_attribute_index(
            section(
                &bytes,
                header.attribute_index_offset,
                header.attribute_index_len,
            )?,
            &header,
        )?;
        Ok(Self {
            backing: Backing::Resident(bytes),
            header,
            record_offsets,
            trace_index,
            attribute_index,
            last_query_used_index: AtomicBool::new(false),
        })
    }

    /// Opens a v2 segment FILE-BACKED: only the header and index sections are
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
    /// v3 it did, and an indexed prompt cost its own text; v4 keys the
    /// attribute index on a digest instead. [`Self::approx_index_bytes`]
    /// measures the result.
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
        let trace_bytes = read_section(header.trace_index_offset, header.trace_index_len)?;
        let trace_index = decode_string_index(&trace_bytes, false, header.record_count)?
            .into_iter()
            .map(|((key, _), offsets)| (key, offsets))
            .collect();
        let attribute_bytes =
            read_section(header.attribute_index_offset, header.attribute_index_len)?;
        let attribute_index = read_attribute_index(&attribute_bytes, &header)?;
        Ok(Self {
            backing: Backing::File {
                file: std::sync::Mutex::new(file),
                len: total,
            },
            header,
            record_offsets,
            trace_index,
            attribute_index,
            last_query_used_index: AtomicBool::new(false),
        })
    }

    /// Bytes of the payload encoding currently resident in memory: the whole
    /// file for `from_bytes` segments, zero for file-backed ones.
    pub fn resident_bytes(&self) -> usize {
        self.backing.resident_len()
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
        let traces = hash_table_bytes::<String, Vec<u64>>(self.trace_index.capacity())
            + self
                .trace_index
                .iter()
                .map(|(key, postings)| {
                    key.capacity() + postings.capacity() * std::mem::size_of::<u64>()
                })
                .sum::<usize>();
        offsets + traces + self.attribute_index.approx_bytes()
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
        self.last_query_used_index.store(true, Ordering::Relaxed);
        match self.record_offsets.get(ordinal) {
            Some(offset) => self.decode_at(*offset).map(Some),
            None => Ok(None),
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
    /// is then checked against the value actually stored in the record. A
    /// digest collision therefore costs a wasted decode and cannot produce a
    /// wrong row.
    pub fn query_attribute(&self, key: &str, value: &str) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.store(true, Ordering::Relaxed);
        let mut records = Vec::new();
        for offset in self.attribute_index.candidates(key, value) {
            let record = self.decode_at(*offset)?;
            if record.attributes.get(key).map(String::as_str) == Some(value) {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.timestamp);
        Ok(records)
    }

    /// Returns records in the inclusive timestamp range in stable timestamp order.
    pub fn query_time_range(&self, start: u64, end: u64) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.store(false, Ordering::Relaxed);
        if start > end {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for offset in &self.record_offsets {
            let record = self.decode_at(*offset)?;
            if record.timestamp >= start && record.timestamp <= end {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.timestamp);
        Ok(records)
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

    /// Inclusive `(min, max)` record timestamp, or `None` when the segment
    /// predates the field and its range is therefore unknown.
    ///
    /// Callers must treat `None` as "cannot rule this segment out". Reading it
    /// as an empty range would silently drop results from every v2 segment in
    /// the store.
    pub fn timestamp_range(&self) -> Option<(u64, u64)> {
        self.header().timestamps
    }

    /// Whether any record here can fall inside `[since, until]`.
    ///
    /// This is the whole point of the v3 range: a query that answers `false`
    /// skips the segment without reading a byte of it.
    pub fn may_contain_timestamps(&self, since: Option<u64>, until: Option<u64>) -> bool {
        let Some((min, max)) = self.timestamp_range() else {
            // Unknown range: it might. See `timestamp_range`.
            return true;
        };
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

    /// Timestamp of the record at a posting offset without decoding it: the
    /// timestamp is the record's first fixed field.
    pub fn timestamp_at(&self, relative_offset: u64) -> Result<u64, Error> {
        if relative_offset >= self.header.records_len {
            return Err(Error::Corrupt("record offset is outside record region"));
        }
        let absolute = self
            .header
            .records_offset
            .checked_add(relative_offset)
            .ok_or(Error::Corrupt("record offset overflow"))?;
        let bytes = self.backing.read_range(absolute, 8)?;
        Ok(u64::from_le_bytes(
            bytes.as_slice().try_into().expect("8 bytes"),
        ))
    }

    /// Decodes exactly one record at a posting offset.
    pub fn record_at_offset(&self, relative_offset: u64) -> Result<Record, Error> {
        self.decode_at(relative_offset)
    }

    fn decode_postings(&self, postings: Option<&Vec<u64>>) -> Result<Vec<Record>, Error> {
        let mut records = Vec::new();
        if let Some(postings) = postings {
            for offset in postings {
                records.push(self.decode_at(*offset)?);
            }
        }
        records.sort_by_key(|record| record.timestamp);
        Ok(records)
    }

    fn decode_at(&self, relative_offset: u64) -> Result<Record, Error> {
        if relative_offset >= self.header.records_len {
            return Err(Error::Corrupt("record offset is outside record region"));
        }
        let absolute = self
            .header
            .records_offset
            .checked_add(relative_offset)
            .ok_or(Error::Corrupt("record offset overflow"))?;
        // Exact record length from the consecutive-offsets invariant: read
        // precisely one record's bytes, resident or from disk.
        let position = self
            .record_offsets
            .binary_search(&relative_offset)
            .map_err(|_| Error::Corrupt("offset is not a record boundary"))?;
        let record_len = self
            .record_offsets
            .get(position + 1)
            .copied()
            .unwrap_or(self.header.records_len)
            .checked_sub(relative_offset)
            .ok_or(Error::Corrupt("record length underflow"))?;
        let buffer = self.backing.read_range(absolute, record_len)?;
        decode_record(&buffer, 0, buffer.len() as u64)
    }
}

/// Encodes records into a complete segment byte stream.
pub fn encode(records: &[RecordInput]) -> Result<Vec<u8>, Error> {
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
        encode_record(&mut record_region, record)?;
        trace_index
            .entry((record.trace_id.clone(), String::new()))
            .or_default()
            .push(offset);
        for (key, value) in &record.attributes {
            let id = *key_ids
                .get(key.as_str())
                .expect("every attribute key is in the dictionary");
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
    let trace_region = encode_string_index(&trace_index, false)?;
    let attribute_region = encode_attribute_index(&attribute_keys, &attribute_index)?;

    let records_offset = HEADER_LEN as u64;
    let offsets_offset = records_offset + record_region.len() as u64;
    let trace_index_offset = offsets_offset + offset_region.len() as u64;
    let attribute_index_offset = trace_index_offset + trace_region.len() as u64;

    let mut bytes = Vec::with_capacity(
        HEADER_LEN
            + record_region.len()
            + offset_region.len()
            + trace_region.len()
            + attribute_region.len(),
    );
    bytes.extend_from_slice(&MAGIC);
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
    // Inclusive timestamp range. An empty segment gets an empty range
    // (min > max), which can never overlap a query and is therefore always
    // skippable — the correct answer for a segment holding nothing.
    let (min_ts, max_ts) = records.iter().fold((u64::MAX, 0_u64), |(lo, hi), record| {
        (lo.min(record.timestamp), hi.max(record.timestamp))
    });
    put_u64(&mut bytes, min_ts);
    put_u64(&mut bytes, max_ts);
    debug_assert_eq!(bytes.len(), HEADER_LEN);
    bytes.extend_from_slice(&record_region);
    bytes.extend_from_slice(&offset_region);
    bytes.extend_from_slice(&trace_region);
    bytes.extend_from_slice(&attribute_region);
    Ok(bytes)
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

fn encode_record(output: &mut Vec<u8>, record: &RecordInput) -> Result<(), Error> {
    let trace_len =
        u32::try_from(record.trace_id.len()).map_err(|_| Error::TooLarge("trace id"))?;
    let attribute_count =
        u32::try_from(record.attributes.len()).map_err(|_| Error::TooLarge("attribute count"))?;
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| Error::TooLarge("payload"))?;
    put_u64(output, record.timestamp);
    put_u32(output, trace_len);
    put_u32(output, attribute_count);
    put_u32(output, payload_len);
    put_u32(output, 0);
    output.extend_from_slice(record.trace_id.as_bytes());
    for (key, value) in &record.attributes {
        put_len_bytes(output, key.as_bytes(), "attribute key")?;
        put_len_bytes(output, value.as_bytes(), "attribute value")?;
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
    let mut attributes = BTreeMap::new();
    for _ in 0..attribute_count {
        let key = take_len_bytes(bytes, &mut cursor, region_end)?;
        let value = take_len_bytes(bytes, &mut cursor, region_end)?;
        attributes.insert(
            std::str::from_utf8(key)?.to_owned(),
            std::str::from_utf8(value)?.to_owned(),
        );
    }
    let payload = take(bytes, &mut cursor, payload_len, region_end)?.to_vec();
    Ok(Record {
        timestamp,
        trace_id,
        attributes,
        payload,
    })
}

fn validate_record_offsets(bytes: &[u8], header: &Header, offsets: &[u64]) -> Result<(), Error> {
    let mut previous = None;
    for offset in offsets {
        if *offset >= header.records_len || previous.is_some_and(|value| *offset <= value) {
            return Err(Error::Corrupt("record offsets are invalid or unordered"));
        }
        decode_record(
            bytes,
            (header.records_offset + *offset) as usize,
            header.records_offset + header.records_len,
        )?;
        previous = Some(*offset);
    }
    if offsets.is_empty() && header.records_len != 0 {
        return Err(Error::Corrupt("record region exists without records"));
    }
    Ok(())
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
/// bounds only. The file-backed open validates records lazily on access —
/// a corrupt record surfaces as Error::Corrupt from that read.
fn validate_record_offsets_lengths(header: &Header, offsets: &[u64]) -> Result<(), Error> {
    let mut previous = None;
    for offset in offsets {
        if *offset >= header.records_len || previous.is_some_and(|value| *offset <= value) {
            return Err(Error::Corrupt("record offsets are invalid or unordered"));
        }
        previous = Some(*offset);
    }
    if offsets.is_empty() && header.records_len != 0 {
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

/// Encodes the v4 attribute section: a key dictionary, then one entry per
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
        u32::try_from(keys.len()).map_err(|_| Error::TooLarge("attribute key dictionary"))?,
    );
    for key in keys {
        put_len_bytes(&mut output, key.as_bytes(), "index key")?;
    }
    put_u32(
        &mut output,
        u32::try_from(postings.len()).map_err(|_| Error::TooLarge("index"))?,
    );
    for ((key_id, digest), offsets) in postings {
        put_u32(&mut output, *key_id);
        output.extend_from_slice(digest.as_bytes());
        put_u32(
            &mut output,
            u32::try_from(offsets.len()).map_err(|_| Error::TooLarge("postings"))?,
        );
        for offset in offsets {
            put_u64(&mut output, *offset);
        }
    }
    Ok(output)
}

/// Decodes the v4 attribute section.
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

/// Decodes a v2/v3 attribute section — which stores value TEXT — into the v4
/// resident form by hashing each value as it is read.
///
/// The text is not retained. It is, however, materialized transiently: peak
/// memory while opening an old segment is still the old cost, and only the
/// steady state improves. That is the price of not rewriting files on
/// upgrade, and it is bounded by one segment rather than by the store.
fn upgrade_attribute_index(data: &[u8], record_count: u64) -> Result<AttributeIndex, Error> {
    let legacy = decode_string_index(data, true, record_count)?;
    let mut index = AttributeIndex::default();
    for ((key, value), offsets) in legacy {
        let id = match index.key_ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = u32::try_from(index.keys.len())
                    .map_err(|_| Error::TooLarge("attribute key dictionary"))?;
                index.key_ids.insert(key.clone(), id);
                index.keys.push(key.clone());
                id
            }
        };
        let digest = hash_attribute(&key, &value);
        // A v2/v3 section cannot contain a duplicate (key, value) — the
        // legacy decoder rejects that — so a collision here would be a real
        // digest collision. Merge rather than drop: the postings must stay
        // complete, and verification downstream sorts out which record
        // actually holds which value.
        index
            .postings
            .entry((id, digest))
            .or_insert_with(Vec::new)
            .extend_from_slice(&offsets);
    }
    for offsets in index.postings.values_mut() {
        offsets.sort_unstable();
        offsets.dedup();
        offsets.shrink_to_fit();
    }
    Ok(index)
}

/// Reads the attribute section in whichever encoding the header declares.
fn read_attribute_index(data: &[u8], header: &Header) -> Result<AttributeIndex, Error> {
    if header.version >= 4 {
        decode_attribute_index(data, header.record_count)
    } else {
        upgrade_attribute_index(data, header.record_count)
    }
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
        u32::try_from(index.len()).map_err(|_| Error::TooLarge("index"))?,
    );
    for ((key, value), postings) in index {
        put_len_bytes(&mut output, key.as_bytes(), "index key")?;
        if include_value {
            put_len_bytes(&mut output, value.as_bytes(), "index value")?;
        }
        put_u32(
            &mut output,
            u32::try_from(postings.len()).map_err(|_| Error::TooLarge("postings"))?,
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
        u32::try_from(bytes.len()).map_err(|_| Error::TooLarge(field))?,
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
