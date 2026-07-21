//! A compact tracing datastore for persisting and querying spans.
//!
//! The crate provides the storage engine used by the bundled HTTP server and
//! benchmark executable.
//!
//! # Example
//!
//! ```text
//! // See the public storage types below and the README for complete setup and
//! // ingestion examples for `traza`.
//! ```

#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

const SEGMENT_MAGIC: &[u8; 8] = b"TRCSEG01";
const INDEX_MAGIC: &[u8; 8] = b"TRCIDX01";
const MANIFEST_VERSION: u32 = 1;

/// Represents result data used by the tracing datastore.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
/// Represents error data used by the tracing datastore.
pub enum Error {
    /// Represents the io case.
    Io(io::Error),
    /// Represents the json case.
    Json(serde_json::Error),
    /// Represents the invaliddata case.
    InvalidData(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::InvalidData(message) => write!(f, "invalid data: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
/// Represents event data used by the tracing datastore.
pub struct Event {
    /// The name value.
    pub name: String,
    /// The timestamp ns value.
    pub timestamp_ns: u64,
    #[serde(default)]
    /// The attributes value.
    pub attributes: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
/// Represents span data used by the tracing datastore.
pub struct Span {
    /// The trace id value.
    pub trace_id: String,
    /// The span id value.
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The parent span id value.
    pub parent_span_id: Option<String>,
    /// The name value.
    pub name: String,
    /// The start time ns value.
    pub start_time_ns: u64,
    /// The end time ns value.
    pub end_time_ns: u64,
    #[serde(default)]
    /// The status value.
    pub status: String,
    #[serde(default)]
    /// The service value.
    pub service: String,
    #[serde(default)]
    /// The attributes value.
    pub attributes: Map<String, Value>,
    #[serde(default)]
    /// The events value.
    pub events: Vec<Event>,
}

impl Span {
    /// Performs the `duration_ns` datastore operation.
    pub fn duration_ns(&self) -> u64 {
        self.end_time_ns.saturating_sub(self.start_time_ns)
    }
}

#[derive(Clone, Debug, Default)]
/// Represents spanfilter data used by the tracing datastore.
pub struct SpanFilter {
    /// The service value.
    pub service: Option<String>,
    /// The name value.
    pub name: Option<String>,
    /// The attributes value.
    pub attributes: Vec<(String, Value)>,
    /// The min duration ns value.
    pub min_duration_ns: Option<u64>,
    /// The since ns value.
    pub since_ns: Option<u64>,
    /// The until ns value.
    pub until_ns: Option<u64>,
    /// The limit value.
    pub limit: Option<usize>,
}

impl SpanFilter {
    /// Performs the `matches` datastore operation.
    pub fn matches(&self, span: &Span) -> bool {
        if self
            .service
            .as_ref()
            .is_some_and(|value| span.service != *value)
        {
            return false;
        }
        if self.name.as_ref().is_some_and(|value| span.name != *value) {
            return false;
        }
        if self
            .attributes
            .iter()
            .any(|(key, value)| span.attributes.get(key) != Some(value))
        {
            return false;
        }
        if self
            .min_duration_ns
            .is_some_and(|value| span.duration_ns() < value)
        {
            return false;
        }
        if self
            .since_ns
            .is_some_and(|value| span.start_time_ns < value)
        {
            return false;
        }
        if self
            .until_ns
            .is_some_and(|value| span.start_time_ns > value)
        {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, Serialize)]
/// Represents trace data used by the tracing datastore.
pub struct Trace {
    /// The trace id value.
    pub trace_id: String,
    /// The spans value.
    pub spans: Vec<Span>,
}

#[derive(Clone, Copy, Debug, Serialize)]
/// Represents stats data used by the tracing datastore.
pub struct Stats {
    /// The span count value.
    pub span_count: u64,
    /// The segment count value.
    pub segment_count: u64,
    /// The bytes on disk value.
    pub bytes_on_disk: u64,
}

#[derive(Clone, Debug)]
/// Represents config data used by the tracing datastore.
pub struct Config {
    /// The flush spans value.
    pub flush_spans: usize,
    /// The ttl seconds value.
    pub ttl_seconds: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            flush_spans: 10_000,
            ttl_seconds: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    next_sequence: u64,
    segments: Vec<ManifestSegment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestSegment {
    file: String,
    span_count: u64,
    min_start_ns: u64,
    max_start_ns: u64,
    max_end_ns: u64,
    bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiskIndex {
    offsets: Vec<u64>,
    traces: HashMap<String, Vec<u32>>,
    services: HashMap<String, Vec<u32>>,
    names: HashMap<String, Vec<u32>>,
    attributes: HashMap<String, HashMap<String, Vec<u32>>>,
}

#[derive(Clone, Debug)]
struct Segment {
    metadata: ManifestSegment,
    index: DiskIndex,
}

struct WriterState {
    buffer: Vec<Span>,
    next_sequence: u64,
}

/// Represents store data used by the tracing datastore.
pub struct Store {
    data_dir: PathBuf,
    config: Config,
    writer: Mutex<WriterState>,
    segments: RwLock<Vec<Segment>>,
}

impl Store {
    /// Opens a tracing datastore at the supplied path.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        if config.flush_spans == 0 {
            return Err(Error::InvalidData("flush_spans must be positive".into()));
        }
        let data_dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;
        let manifest_path = data_dir.join("MANIFEST.json");
        let manifest = if manifest_path.exists() {
            let reader = BufReader::new(File::open(&manifest_path)?);
            let manifest: Manifest = serde_json::from_reader(reader)?;
            if manifest.version != MANIFEST_VERSION {
                return Err(Error::InvalidData(format!(
                    "unsupported manifest version {}",
                    manifest.version
                )));
            }
            manifest
        } else {
            let manifest = Manifest {
                version: MANIFEST_VERSION,
                next_sequence: 1,
                segments: Vec::new(),
            };
            publish_manifest(&data_dir, &manifest)?;
            manifest
        };

        let mut segments = Vec::with_capacity(manifest.segments.len());
        for metadata in &manifest.segments {
            let index = read_segment_index(&data_dir.join(&metadata.file))?;
            if index.offsets.len() as u64 != metadata.span_count {
                return Err(Error::InvalidData(format!(
                    "segment {} count does not match its index",
                    metadata.file
                )));
            }
            segments.push(Segment {
                metadata: metadata.clone(),
                index,
            });
        }

        Ok(Self {
            data_dir,
            config,
            writer: Mutex::new(WriterState {
                buffer: Vec::new(),
                next_sequence: manifest.next_sequence,
            }),
            segments: RwLock::new(segments),
        })
    }

    /// Performs the `ingest` datastore operation.
    pub fn ingest(&self, span: Span) -> Result<()> {
        self.ingest_batch(vec![span])
    }

    /// Performs the `ingest_batch` datastore operation.
    pub fn ingest_batch(&self, spans: Vec<Span>) -> Result<()> {
        if spans.is_empty() {
            return Ok(());
        }
        let should_flush = {
            let mut writer = self.writer.lock().expect("writer mutex poisoned");
            writer.buffer.extend(spans);
            writer.buffer.len() >= self.config.flush_spans
        };
        if should_flush {
            self.flush()?;
        }
        Ok(())
    }

    /// Performs the `buffered_span_count` datastore operation.
    pub fn buffered_span_count(&self) -> usize {
        self.writer
            .lock()
            .expect("writer mutex poisoned")
            .buffer
            .len()
    }

    /// Flushes buffered datastore state to durable storage.
    pub fn flush(&self) -> Result<()> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        if writer.buffer.is_empty() {
            return Ok(());
        }
        writer.buffer.sort_unstable_by(|left, right| {
            left.trace_id
                .cmp(&right.trace_id)
                .then(left.start_time_ns.cmp(&right.start_time_ns))
                .then(left.span_id.cmp(&right.span_id))
        });

        let sequence = writer.next_sequence;
        let file_name = format!("segment-{sequence:020}.seg");
        let temporary_name = format!(".{file_name}.tmp");
        let temporary_path = self.data_dir.join(&temporary_name);
        let final_path = self.data_dir.join(&file_name);
        let (mut metadata, index) = write_segment(&temporary_path, &writer.buffer)?;
        metadata.file = file_name;
        fs::rename(&temporary_path, &final_path)?;
        sync_directory(&self.data_dir)?;
        metadata.bytes = fs::metadata(&final_path)?.len();

        let mut segments = self.segments.write().expect("segments lock poisoned");
        let mut manifest_segments: Vec<_> = segments
            .iter()
            .map(|segment| segment.metadata.clone())
            .collect();
        manifest_segments.push(metadata.clone());
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            next_sequence: sequence + 1,
            segments: manifest_segments,
        };
        publish_manifest(&self.data_dir, &manifest)?;

        segments.push(Segment { metadata, index });
        writer.next_sequence += 1;
        writer.buffer.clear();
        Ok(())
    }

    /// Returns data selected by the `get_trace` operation.
    pub fn get_trace(&self, trace_id: &str) -> Result<Option<Trace>> {
        let mut spans = Vec::new();
        {
            let segments = self.segments.read().expect("segments lock poisoned");
            for segment in segments.iter() {
                let Some(positions) = segment.index.traces.get(trace_id) else {
                    continue;
                };
                spans.extend(read_positions(
                    &self.data_dir.join(&segment.metadata.file),
                    &segment.index,
                    positions,
                )?);
            }
        }
        spans.extend(
            self.writer
                .lock()
                .expect("writer mutex poisoned")
                .buffer
                .iter()
                .filter(|span| span.trace_id == trace_id)
                .cloned(),
        );
        if spans.is_empty() {
            return Ok(None);
        }
        spans.sort_unstable_by(|left, right| {
            left.start_time_ns
                .cmp(&right.start_time_ns)
                .then(left.span_id.cmp(&right.span_id))
        });
        Ok(Some(Trace {
            trace_id: trace_id.to_owned(),
            spans,
        }))
    }

    /// Performs the `query` datastore operation.
    pub fn query(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        let limit = filter.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut matches = Vec::new();
        {
            let segments = self.segments.read().expect("segments lock poisoned");
            for segment in segments.iter() {
                if filter
                    .since_ns
                    .is_some_and(|since| segment.metadata.max_start_ns < since)
                    || filter
                        .until_ns
                        .is_some_and(|until| segment.metadata.min_start_ns > until)
                {
                    continue;
                }
                let positions = candidate_positions(&segment.index, filter);
                if positions.is_empty() {
                    continue;
                }
                for span in read_positions(
                    &self.data_dir.join(&segment.metadata.file),
                    &segment.index,
                    &positions,
                )? {
                    if filter.matches(&span) {
                        matches.push(span);
                    }
                }
            }
        }
        matches.extend(
            self.writer
                .lock()
                .expect("writer mutex poisoned")
                .buffer
                .iter()
                .filter(|span| filter.matches(span))
                .cloned(),
        );
        matches.sort_unstable_by(|left, right| {
            left.start_time_ns
                .cmp(&right.start_time_ns)
                .then(left.trace_id.cmp(&right.trace_id))
                .then(left.span_id.cmp(&right.span_id))
        });
        matches.truncate(limit);
        Ok(matches)
    }

    /// Returns current datastore statistics.
    pub fn stats(&self) -> Result<Stats> {
        let segments = self.segments.read().expect("segments lock poisoned");
        let buffered = self
            .writer
            .lock()
            .expect("writer mutex poisoned")
            .buffer
            .len() as u64;
        Ok(Stats {
            span_count: segments
                .iter()
                .map(|segment| segment.metadata.span_count)
                .sum::<u64>()
                + buffered,
            segment_count: segments.len() as u64,
            bytes_on_disk: directory_bytes(&self.data_dir)?,
        })
    }

    /// Performs the `compact_expired` datastore operation.
    pub fn compact_expired(&self) -> Result<usize> {
        let Some(ttl_seconds) = self.config.ttl_seconds else {
            return Ok(0);
        };
        self.expire_before(now_ns().saturating_sub(ttl_seconds.saturating_mul(1_000_000_000)))
    }

    /// Removes spans older than the supplied cutoff.
    pub fn expire_before(&self, cutoff_ns: u64) -> Result<usize> {
        let _writer = self.writer.lock().expect("writer mutex poisoned");
        let mut segments = self.segments.write().expect("segments lock poisoned");
        let retained: Vec<_> = segments
            .iter()
            .filter(|segment| segment.metadata.max_end_ns >= cutoff_ns)
            .cloned()
            .collect();
        let expired: Vec<_> = segments
            .iter()
            .filter(|segment| segment.metadata.max_end_ns < cutoff_ns)
            .map(|segment| segment.metadata.file.clone())
            .collect();
        if expired.is_empty() {
            return Ok(0);
        }

        let next_sequence = segments
            .iter()
            .filter_map(|segment| sequence_from_name(&segment.metadata.file))
            .max()
            .unwrap_or(0)
            + 1;
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            next_sequence,
            segments: retained
                .iter()
                .map(|segment| segment.metadata.clone())
                .collect(),
        };
        publish_manifest(&self.data_dir, &manifest)?;
        *segments = retained;

        for file in &expired {
            match fs::remove_file(self.data_dir.join(file)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        sync_directory(&self.data_dir)?;
        Ok(expired.len())
    }
}

fn candidate_positions(index: &DiskIndex, filter: &SpanFilter) -> Vec<u32> {
    let mut sets: Vec<&Vec<u32>> = Vec::new();
    if let Some(service) = &filter.service {
        let Some(values) = index.services.get(service) else {
            return Vec::new();
        };
        sets.push(values);
    }
    if let Some(name) = &filter.name {
        let Some(values) = index.names.get(name) else {
            return Vec::new();
        };
        sets.push(values);
    }
    for (key, value) in &filter.attributes {
        let Some(values) = index
            .attributes
            .get(key)
            .and_then(|values| values.get(&canonical_json(value)))
        else {
            return Vec::new();
        };
        sets.push(values);
    }
    if sets.is_empty() {
        return (0..index.offsets.len() as u32).collect();
    }
    sets.sort_unstable_by_key(|values| values.len());
    let mut candidates: BTreeSet<u32> = sets[0].iter().copied().collect();
    for values in sets.iter().skip(1) {
        let values: BTreeSet<u32> = values.iter().copied().collect();
        candidates.retain(|position| values.contains(position));
        if candidates.is_empty() {
            break;
        }
    }
    candidates.into_iter().collect()
}

fn write_segment(path: &Path, spans: &[Span]) -> Result<(ManifestSegment, DiskIndex)> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(SEGMENT_MAGIC)?;
    write_u64(&mut writer, spans.len() as u64)?;
    let index_offset_position = 16;
    write_u64(&mut writer, 0)?;

    let mut index = DiskIndex {
        offsets: Vec::with_capacity(spans.len()),
        traces: HashMap::new(),
        services: HashMap::new(),
        names: HashMap::new(),
        attributes: HashMap::new(),
    };
    let mut min_start_ns = u64::MAX;
    let mut max_start_ns = 0;
    let mut max_end_ns = 0;
    for (position, span) in spans.iter().enumerate() {
        let offset = writer.stream_position()?;
        index.offsets.push(offset);
        let bytes = serde_json::to_vec(span)?;
        write_u32(&mut writer, bytes.len() as u32)?;
        writer.write_all(&bytes)?;
        let position = position as u32;
        index
            .traces
            .entry(span.trace_id.clone())
            .or_default()
            .push(position);
        index
            .services
            .entry(span.service.clone())
            .or_default()
            .push(position);
        index
            .names
            .entry(span.name.clone())
            .or_default()
            .push(position);
        for (key, value) in &span.attributes {
            index
                .attributes
                .entry(key.clone())
                .or_default()
                .entry(canonical_json(value))
                .or_default()
                .push(position);
        }
        min_start_ns = min_start_ns.min(span.start_time_ns);
        max_start_ns = max_start_ns.max(span.start_time_ns);
        max_end_ns = max_end_ns.max(span.end_time_ns);
    }

    let index_offset = writer.stream_position()?;
    let index_bytes = serde_json::to_vec(&index)?;
    writer.write_all(INDEX_MAGIC)?;
    write_u64(&mut writer, index_bytes.len() as u64)?;
    writer.write_all(&index_bytes)?;
    writer.flush()?;
    let mut file = writer.into_inner().map_err(|error| error.into_error())?;
    file.seek(SeekFrom::Start(index_offset_position))?;
    write_u64(&mut file, index_offset)?;
    file.sync_all()?;

    Ok((
        ManifestSegment {
            file: String::new(),
            span_count: spans.len() as u64,
            min_start_ns: if spans.is_empty() { 0 } else { min_start_ns },
            max_start_ns,
            max_end_ns,
            bytes: 0,
        },
        index,
    ))
}

fn read_segment_index(path: &Path) -> Result<DiskIndex> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut magic = [0_u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != SEGMENT_MAGIC {
        return Err(Error::InvalidData(format!(
            "{} has invalid segment magic",
            path.display()
        )));
    }
    let _count = read_u64(&mut reader)?;
    let index_offset = read_u64(&mut reader)?;
    reader.seek(SeekFrom::Start(index_offset))?;
    reader.read_exact(&mut magic)?;
    if &magic != INDEX_MAGIC {
        return Err(Error::InvalidData(format!(
            "{} has invalid index magic",
            path.display()
        )));
    }
    let length = read_u64(&mut reader)?;
    if length > 1_000_000_000 {
        return Err(Error::InvalidData("index exceeds safety limit".into()));
    }
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_positions(path: &Path, index: &DiskIndex, positions: &[u32]) -> Result<Vec<Span>> {
    let mut file = File::open(path)?;
    let mut spans = Vec::with_capacity(positions.len());
    for &position in positions {
        let offset = *index
            .offsets
            .get(position as usize)
            .ok_or_else(|| Error::InvalidData("index position out of bounds".into()))?;
        file.seek(SeekFrom::Start(offset))?;
        let length = read_u32(&mut file)? as usize;
        if length > 64 * 1024 * 1024 {
            return Err(Error::InvalidData(
                "span record exceeds safety limit".into(),
            ));
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        spans.push(serde_json::from_slice(&bytes)?);
    }
    Ok(spans)
}

fn publish_manifest(data_dir: &Path, manifest: &Manifest) -> Result<()> {
    let temporary_path = data_dir.join(".MANIFEST.json.tmp");
    let final_path = data_dir.join("MANIFEST.json");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary_path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, manifest)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::rename(&temporary_path, &final_path)?;
    sync_directory(data_dir)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut bytes = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes += entry.metadata()?.len();
        }
    }
    Ok(bytes)
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).expect("serializing a JSON value cannot fail")
}

fn sequence_from_name(name: &str) -> Option<u64> {
    name.strip_prefix("segment-")?
        .strip_suffix(".seg")?
        .parse()
        .ok()
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
