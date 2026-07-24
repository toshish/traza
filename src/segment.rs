//! Segment file-backed storage and persisted indexes.
//!
//! The on-disk format is version 2 (see `MAGIC`/`VERSION`); version 1 (JSONL)
//! is no longer read.
//!
//! An opened file-backed segment owns only its file handle and decoded index
//! maps. In-memory segments built for encoding may own their bytes. Records are
//! decoded only when a query selects their offsets; no decoded record vector
//! is retained by [`Segment`].

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// Eight-byte marker at the beginning of every segment file. The version
/// lives in [`VERSION`], not in the magic, so the marker itself stays fixed
/// across format revisions.
pub const MAGIC: [u8; 8] = *b"TRAZASEG";
/// On-disk format version written by this module. This — not the magic — is
/// how the format generation is identified; the magic only says "a Traza
/// segment". (JSONL v1 carried no magic; this indexed format is version 2.)
pub const VERSION: u16 = 2;
/// Fixed header size in bytes.
pub const HEADER_LEN: usize = 80;

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
            return Err(Error::Corrupt("file is shorter than the v2 header"));
        }
        if bytes[..8] != MAGIC {
            return Err(Error::Unsupported("not a Traza segment (bad magic)"));
        }
        let version = read_u16(bytes, 8)?;
        if version != VERSION {
            return Err(Error::Unsupported("unsupported segment version"));
        }
        let header_len = read_u16(bytes, 10)?;
        if usize::from(header_len) != HEADER_LEN {
            return Err(Error::Corrupt("invalid v2 header length"));
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

/// An opened v2 segment backed by either a file or encoded memory.
///
/// File-backed segments retain only offsets and persisted index postings.
/// Query results are decoded from their exact byte ranges on demand.
#[derive(Debug)]
pub struct Segment {
    backing: Backing,
    header: Header,
    record_offsets: Vec<u64>,
    trace_index: HashMap<String, Vec<u64>>,
    attribute_index: HashMap<(String, String), Vec<u64>>,
    last_query_used_index: Cell<bool>,
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
        let attribute_index = decode_string_index(
            section(
                &bytes,
                header.attribute_index_offset,
                header.attribute_index_len,
            )?,
            true,
            header.record_count,
        )?;
        Ok(Self {
            backing: Backing::Resident(bytes),
            header,
            record_offsets,
            trace_index,
            attribute_index,
            last_query_used_index: Cell::new(false),
        })
    }

    /// Opens a v2 segment FILE-BACKED: only the header and index sections are
    /// read into memory; record payloads stay on disk and are fetched on
    /// demand. This is the larger-than-RAM path — resident cost is O(indexes).
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
        let attribute_index = decode_string_index(&attribute_bytes, true, header.record_count)?;
        Ok(Self {
            backing: Backing::File {
                file: std::sync::Mutex::new(file),
                len: total,
            },
            header,
            record_offsets,
            trace_index,
            attribute_index,
            last_query_used_index: Cell::new(false),
        })
    }

    /// Bytes of the payload encoding currently resident in memory: the whole
    /// file for `from_bytes` segments, zero for file-backed ones.
    pub fn resident_bytes(&self) -> usize {
        self.backing.resident_len()
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
        self.last_query_used_index.get()
    }

    /// Decodes one record by ordinal through the persisted offset table.
    pub fn record(&self, ordinal: usize) -> Result<Option<Record>, Error> {
        self.last_query_used_index.set(true);
        match self.record_offsets.get(ordinal) {
            Some(offset) => self.decode_at(*offset).map(Some),
            None => Ok(None),
        }
    }

    /// Looks up records for an exact trace identifier.
    pub fn query_trace(&self, trace_id: &str) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.set(true);
        self.decode_postings(self.trace_index.get(trace_id))
    }

    /// Looks up records for an exact attribute key/value pair.
    pub fn query_attribute(&self, key: &str, value: &str) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.set(true);
        self.decode_postings(
            self.attribute_index
                .get(&(key.to_owned(), value.to_owned())),
        )
    }

    /// Returns records in the inclusive timestamp range in stable timestamp order.
    pub fn query_time_range(&self, start: u64, end: u64) -> Result<Vec<Record>, Error> {
        self.last_query_used_index.set(false);
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

    /// All record offsets in record (timestamp) order — the lazy full-scan
    /// candidate list when no index applies.
    pub fn record_offsets(&self) -> &[u64] {
        &self.record_offsets
    }

    /// Raw posting offsets for one attribute key/value pair, in record
    /// (timestamp) order — no records are decoded. The lazy query path pairs
    /// this with [`Self::timestamp_at`] and [`Self::record_at_offset`] so a
    /// limited query decodes only the records it returns.
    pub fn attribute_posting_offsets(&self, key: &str, value: &str) -> Vec<u64> {
        self.attribute_posting_offsets_ref(key, value).to_vec()
    }

    /// Borrowed posting offsets for one attribute key/value pair.
    ///
    /// This avoids cloning a potentially corpus-sized posting list for each
    /// bounded query page.
    pub fn attribute_posting_offsets_ref(&self, key: &str, value: &str) -> &[u64] {
        self.last_query_used_index.set(true);
        self.attribute_index
            .get(&(key.to_owned(), value.to_owned()))
            .map_or(&[], Vec::as_slice)
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
    let mut attribute_index: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();

    for record in records {
        let offset = record_region.len() as u64;
        offsets.push(offset);
        encode_record(&mut record_region, record)?;
        trace_index
            .entry((record.trace_id.clone(), String::new()))
            .or_default()
            .push(offset);
        for (key, value) in &record.attributes {
            attribute_index
                .entry((key.clone(), value.clone()))
                .or_default()
                .push(offset);
        }
    }

    let mut offset_region = Vec::with_capacity(offsets.len() * 8);
    for offset in &offsets {
        put_u64(&mut offset_region, *offset);
    }
    let trace_region = encode_string_index(&trace_index, false)?;
    let attribute_region = encode_string_index(&attribute_index, true)?;

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
