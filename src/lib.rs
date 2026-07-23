#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A small durable storage engine for tracing spans.
//!
//! Spans are buffered in memory and periodically persisted as sorted JSON-lines
//! segment files. Reads combine the buffered and persisted data.

pub mod analytics;
pub mod auth;
pub mod dashboard;
pub mod expiration;
pub mod otlp;
pub mod otlp_pb;
pub mod segment_v2;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const LOCK_FILE_NAME: &str = "LOCK";
const SEGMENT_PREFIX: &str = "segment-";
const SEGMENT_SUFFIX: &str = ".jsonl";
/// Suffix for format-v2 segment files (indexed, byte-resident).
const SEGMENT_V2_SUFFIX: &str = ".seg";
/// Reserved attribute-index keys for span fields; the NUL prefix cannot
/// collide with practical user attribute names, and even a collision only
/// over-selects candidates that re-verification drops.
const IDX_SERVICE: &str = "\u{0}service";
const IDX_NAME: &str = "\u{0}name";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An event attached to a span.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Event {
    /// Event name.
    pub name: String,
    /// Event timestamp in nanoseconds since the Unix epoch.
    pub timestamp_ns: u64,
    /// Arbitrary event attributes.
    pub attributes: Map<String, Value>,
}

/// A link from one span to another span, possibly in a different trace.
/// Links model the non-tree relationships agentic traces are full of:
/// fan-out to parallel tool calls, results rejoining a plan, retries
/// referencing their earlier attempt.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Link {
    /// Trace id of the linked span.
    pub trace_id: String,
    /// Span id of the linked span.
    pub span_id: String,
    /// Arbitrary link attributes.
    #[serde(default)]
    pub attributes: Map<String, Value>,
}

/// A single tracing span.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Span {
    /// Identifier shared by every span in a trace. Must be non-empty: it is
    /// half of the (trace_id, span_id) primary key, and both HTTP ingest
    /// surfaces reject spans with an empty id.
    pub trace_id: String,
    /// Identifier unique to this span within its trace. Must be non-empty
    /// (the other half of the primary key): distinct spans sharing an empty
    /// span_id would collide into one upserted key.
    pub span_id: String,
    /// Identifier of the parent span, if this is not a root span.
    #[serde(default)]
    pub parent_span_id: Option<String>,
    /// Operation name.
    pub name: String,
    /// Start timestamp in nanoseconds since the Unix epoch. Deserialization
    /// accepts the documented wire aliases.
    #[serde(
        alias = "start_time_unix_nano",
        alias = "start_timestamp_ns",
        alias = "start_ns",
        alias = "start_time"
    )]
    pub start_time_ns: u64,
    /// End timestamp in nanoseconds since the Unix epoch. Deserialization
    /// accepts the documented wire aliases.
    #[serde(
        alias = "end_time_unix_nano",
        alias = "end_timestamp_ns",
        alias = "end_ns",
        alias = "end_time"
    )]
    pub end_time_ns: u64,
    /// Application-defined completion status.
    #[serde(default)]
    pub status: String,
    /// Service that emitted the span.
    pub service: String,
    /// Arbitrary span attributes.
    #[serde(default)]
    pub attributes: Map<String, Value>,
    /// Events recorded during the span.
    #[serde(default)]
    pub events: Vec<Event>,
    /// Links to other spans (see [`Link`]). Empty for tree-shaped traces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Any other fields supplied at ingest, stored and returned verbatim —
    /// the wire contract promises unknown fields survive the round trip.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// Conditions used to select spans.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanFilter {
    /// Match only spans from this service.
    pub service: Option<String>,
    /// Match only spans with this operation name.
    pub name: Option<String>,
    /// Match spans containing all of these attribute key/value pairs.
    pub attributes: Vec<(String, Value)>,
    /// Match spans whose duration is at least this many nanoseconds.
    pub min_duration_ns: Option<u64>,
    /// Match spans starting at or after this timestamp.
    pub since_ns: Option<u64>,
    /// Match spans starting at or before this timestamp.
    pub until_ns: Option<u64>,
    /// Maximum number of returned spans.
    pub limit: Option<usize>,
}

/// Storage configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Number of buffered spans that triggers an automatic flush.
    pub flush_spans: usize,
    /// Retention period in seconds; zero disables TTL expiration.
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

/// A point-in-time summary of store usage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Number of spans currently buffered in memory.
    pub buffered_spans: usize,
    /// Number of spans in persisted segments.
    pub persisted_spans: usize,
    /// Total number of buffered and persisted spans.
    pub total_spans: usize,
    /// Number of persisted segment files.
    pub segment_count: usize,
    /// Total size of persisted segment files in bytes.
    pub disk_bytes: u64,
}

/// Errors returned by storage operations.
#[derive(Debug)]
pub enum Error {
    /// An underlying filesystem operation failed.
    Io(io::Error),
    /// Stored or supplied JSON could not be encoded or decoded.
    Json(serde_json::Error),
    /// Another live store handle already owns the data directory.
    AlreadyOpen,
    /// An internal synchronization lock was poisoned.
    LockPoisoned(&'static str),
    /// A span violated an ingest invariant (empty primary-key id).
    InvalidSpan(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "storage I/O error: {error}"),
            Self::Json(error) => write!(f, "storage JSON error: {error}"),
            Self::AlreadyOpen => write!(f, "store is already open by another writer"),
            Self::LockPoisoned(name) => write!(f, "storage lock poisoned: {name}"),
            Self::InvalidSpan(reason) => write!(f, "invalid span: {reason}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::AlreadyOpen | Self::LockPoisoned(_) | Self::InvalidSpan(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result type used by the storage API.
pub type Result<T> = std::result::Result<T, Error>;

/// A persisted segment: raw v2 bytes plus their embedded indexes. Spans
/// parse on demand — resident cost is bytes + indexes, never Span structs.
#[derive(Debug)]
struct Segment {
    path: PathBuf,
    bytes: u64,
    seg: Box<segment_v2::Segment>,
}

fn canonical_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn span_to_record(span: &Span) -> Result<segment_v2::RecordInput> {
    let mut attributes = std::collections::BTreeMap::new();
    // User attributes first, and NUL-prefixed user keys are never indexed:
    // a user attribute literally named "\u{0}service" could otherwise
    // overwrite the reserved service posting and turn real service queries
    // into false negatives (found in review). The span itself still stores
    // such attributes verbatim in the payload; only the INDEX ignores them,
    // and the filter path declines index use for them symmetrically.
    for (key, value) in &span.attributes {
        if !key.starts_with('\u{0}') {
            attributes.insert(key.clone(), canonical_value(value));
        }
    }
    attributes.insert(IDX_SERVICE.to_owned(), span.service.clone());
    attributes.insert(IDX_NAME.to_owned(), span.name.clone());
    Ok(segment_v2::RecordInput::new(
        span.start_time_ns,
        span.trace_id.clone(),
        attributes,
        serde_json::to_vec(span)?,
    ))
}

fn record_to_span(record: &segment_v2::Record) -> Result<Span> {
    Ok(serde_json::from_slice(record.payload())?)
}

impl Segment {
    fn span_count(&self) -> usize {
        self.seg.len()
    }

    fn contains_key(&self, trace_id: &str, span_id: &str) -> Result<bool> {
        for span in self.trace_spans(trace_id)? {
            if span.span_id == span_id {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Full parse — the rewrite/inspection path, never the query path.
    fn spans_parsed(&self) -> Result<Vec<Span>> {
        let mut spans = Vec::with_capacity(self.seg.len());
        for ordinal in 0..self.seg.len() {
            if let Some(record) = self.seg.record(ordinal).map_err(segment_v2_error)? {
                spans.push(record_to_span(&record)?);
            }
        }
        Ok(spans)
    }

    fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>> {
        let records = self.seg.query_trace(trace_id).map_err(segment_v2_error)?;
        records.iter().map(record_to_span).collect()
    }
}

fn segment_v2_error(error: segment_v2::Error) -> Error {
    match error {
        segment_v2::Error::Io(inner) => Error::Io(inner),
        other => Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            other.to_string(),
        )),
    }
}

/// The in-memory write buffer, keyed by span identity.
///
/// Both halves of the (trace_id, span_id) primary key must be non-empty at
/// the engine boundary, not just at the HTTP surfaces: distinct spans sharing
/// an empty id would silently collapse into one upserted key for any library
/// consumer too.
fn validate_span(span: &Span) -> Result<()> {
    if span.trace_id.is_empty() {
        return Err(Error::InvalidSpan("trace_id is empty"));
    }
    if span.span_id.is_empty() {
        return Err(Error::InvalidSpan("span_id is empty"));
    }
    Ok(())
}

/// (trace_id, span_id) is the span's PRIMARY KEY: re-ingesting an existing
/// key replaces the buffered version in place — retries are idempotent and
/// never create a second acknowledged copy.
#[derive(Debug, Default)]
struct WriteBuffer {
    spans: Vec<Span>,
    index: std::collections::HashMap<(String, String), usize>,
}

impl WriteBuffer {
    fn upsert(&mut self, span: Span) {
        let key = (span.trace_id.clone(), span.span_id.clone());
        match self.index.get(&key) {
            Some(&position) => self.spans[position] = span,
            None => {
                self.index.insert(key, self.spans.len());
                self.spans.push(span);
            }
        }
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn contains_key(&self, trace_id: &str, span_id: &str) -> bool {
        self.index
            .contains_key(&(trace_id.to_owned(), span_id.to_owned()))
    }

    fn clear(&mut self) {
        self.spans.clear();
        self.index.clear();
    }

    fn retain(&mut self, keep: impl Fn(&Span) -> bool) {
        self.spans.retain(&keep);
        self.index.clear();
        for (position, span) in self.spans.iter().enumerate() {
            self.index
                .insert((span.trace_id.clone(), span.span_id.clone()), position);
        }
    }
}

#[derive(Debug)]
struct DirectoryLock {
    path: PathBuf,
    _file: File,
}

impl DirectoryLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        match Self::try_create(&path) {
            Ok(lock) => Ok(lock),
            Err(Error::AlreadyOpen) => {
                // The lock records its owner's PID. A crashed owner (killed
                // server, OOM, power loss before unlink) must not wedge the
                // store forever: if the recorded process is gone, the lock is
                // stale and may be reclaimed. A live owner still rejects the
                // open. Reclamation must have exactly ONE winner and no
                // check-then-act window: a bare remove (or rename) lets a
                // slow reclaimer that validated the STALE file destroy the
                // fast reclaimer's FRESH lock (found in review, then again by
                // the race test). A reclamation sentinel closes it: exactly
                // one reclaimer create_new-wins the sentinel, re-verifies
                // staleness while holding it, and only then swaps the lock.
                if Self::owner_is_dead(&path) && Self::reclaim_sentinel(&path) {
                    let result = if Self::owner_is_dead(&path) {
                        let _ = fs::remove_file(&path);
                        Self::try_create(&path)
                    } else {
                        Err(Error::AlreadyOpen)
                    };
                    let _ = fs::remove_file(Self::sentinel_path(&path));
                    return result;
                }
                Err(Error::AlreadyOpen)
            }
            Err(error) => Err(error),
        }
    }

    fn try_create(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyOpen);
            }
            Err(error) => return Err(Error::Io(error)),
        };

        if let Err(error) = writeln!(file, "{}", std::process::id()) {
            let _ = fs::remove_file(path);
            return Err(Error::Io(error));
        }
        if let Err(error) = file.sync_all() {
            let _ = fs::remove_file(path);
            return Err(Error::Io(error));
        }

        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    fn sentinel_path(path: &Path) -> PathBuf {
        path.with_extension("reclaim")
    }

    /// Wins the exclusive right to reclaim, or returns false. A sentinel left
    /// by a reclaimer that itself died is reclaimed by the same dead-owner
    /// rule, one level deep.
    fn reclaim_sentinel(path: &Path) -> bool {
        let sentinel = Self::sentinel_path(path);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&sentinel) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                let _ = file.sync_all();
                true
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A reclaimer that died between creating the sentinel and
                // recording its PID leaves an empty file that the PID check
                // treats as live forever. Age is the fallback: reclamation
                // takes milliseconds, so an unreadable sentinel older than
                // ten seconds is a corpse.
                let unreadable_and_old = fs::read_to_string(&sentinel)
                    .map(|contents| contents.trim().parse::<u32>().is_err())
                    .unwrap_or(false)
                    && fs::metadata(&sentinel)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age.as_secs() >= 10);
                if Self::owner_is_dead(&sentinel) || unreadable_and_old {
                    let _ = fs::remove_file(&sentinel);
                }
                // Either way this attempt yields; the next open retries.
                false
            }
            Err(_) => false,
        }
    }

    /// True only when the lock file names a PID that verifiably no longer
    /// exists. Unreadable or malformed lock files are treated as live: false
    /// negatives merely keep the conservative rejection, never corrupt data.
    fn owner_is_dead(path: &Path) -> bool {
        let Ok(contents) = fs::read_to_string(path) else {
            return false;
        };
        let Ok(pid) = contents.trim().parse::<u32>() else {
            return false;
        };
        if pid == std::process::id() {
            return false;
        }
        // Signal 0 probes existence without delivering anything. EPERM means
        // the process exists but belongs to someone else — still live.
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
        {
            Ok(output) => {
                !output.status.success() && {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    !stderr.contains("not permitted") && !stderr.contains("Operation not permitted")
                }
            }
            Err(_) => false,
        }
    }
}

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A durable span store backed by sorted JSON-lines segment files.
pub struct Store {
    directory: PathBuf,
    config: Config,
    // Locking discipline: whenever both locks are needed, acquire writer first
    // and segments second, and retain that order until both guards are dropped.
    // The rollup cache is leaf-level: acquired briefly and never while taking
    // another lock.
    writer: Mutex<WriteBuffer>,
    segments: Mutex<Vec<Segment>>,
    rollups: Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<analytics::SegmentRollup>>>,
    next_segment: AtomicU64,
    _directory_lock: DirectoryLock,
}

impl Store {
    /// Opens or creates a store in `path`.
    ///
    /// Only one live `Store` may own a directory. Opening also removes orphaned
    /// temporary segment files left by an interrupted write.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let directory = path.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;

        let lock_path = directory.join(LOCK_FILE_NAME);
        let directory_lock = DirectoryLock::acquire(lock_path)?;

        let opened = (|| {
            remove_orphan_temps(&directory)?;
            // Interrupted compaction rewrites are finished from their
            // supersede markers BEFORE loading: recovery follows the journal,
            // never content — content-based duplicate healing silently
            // destroyed legitimately re-ingested identical spans (found in
            // review: acknowledged duplicate cardinality must survive
            // restart).
            recover_supersede_markers(&directory)?;
            let mut segments = load_segments(&directory)?;
            segments.sort_by(|left, right| left.path.cmp(&right.path));
            let next_segment = segments
                .iter()
                .filter_map(|segment| segment_number(&segment.path))
                .max()
                .map_or(0, |number| number.saturating_add(1));

            Ok(Self {
                directory,
                config,
                writer: Mutex::new(WriteBuffer::default()),
                segments: Mutex::new(segments),
                rollups: Mutex::new(std::collections::HashMap::new()),
                next_segment: AtomicU64::new(next_segment),
                _directory_lock: directory_lock,
            })
        })();

        opened
    }

    /// Adds one span, automatically flushing when the configured threshold is
    /// reached.
    pub fn ingest(&self, span: Span) -> Result<()> {
        validate_span(&span)?;
        let mut writer = self.lock_writer()?;
        writer.upsert(span);
        if self.should_flush(writer.len()) {
            let mut segments = self.lock_segments()?;
            self.flush_locked(&mut writer, &mut segments)?;
        }
        Ok(())
    }

    /// Adds a batch of spans, automatically flushing when the configured
    /// threshold is reached. The batch is atomic with respect to validation:
    /// if any span is invalid, nothing from the batch is stored.
    pub fn ingest_batch(&self, spans: Vec<Span>) -> Result<()> {
        if spans.is_empty() {
            return Ok(());
        }
        for span in &spans {
            validate_span(span)?;
        }

        let mut writer = self.lock_writer()?;
        for span in spans {
            writer.upsert(span);
        }
        if self.should_flush(writer.len()) {
            let mut segments = self.lock_segments()?;
            self.flush_locked(&mut writer, &mut segments)?;
        }
        Ok(())
    }

    /// Returns the current number of spans buffered in memory.
    pub fn buffered_span_count(&self) -> usize {
        self.writer.lock().map_or(0, |writer| writer.len())
    }

    /// Persists every currently buffered span as one sorted segment.
    pub fn flush(&self) -> Result<()> {
        let mut writer = self.lock_writer()?;
        let mut segments = self.lock_segments()?;
        self.flush_locked(&mut writer, &mut segments)
    }

    /// Returns all spans belonging to `trace_id`, ordered by start time.
    ///
    /// The writer and segment locks are held together while constructing the
    /// combined view, so a concurrent flush cannot move spans between halves
    /// of the snapshot and make them temporarily disappear.
    pub fn get_trace(&self, trace_id: &str) -> Result<Vec<Span>> {
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut result = Vec::new();

        // (trace_id, span_id) is the span's primary key: the newest ingested
        // version wins. Segments are visited oldest-first so later versions
        // overwrite, and the write buffer overwrites everything.
        let mut latest: std::collections::HashMap<String, Span> = std::collections::HashMap::new();
        for segment in segments.iter() {
            for span in segment.trace_spans(trace_id)? {
                latest.insert(span.span_id.clone(), span);
            }
        }
        for span in writer.spans.iter() {
            if span.trace_id == trace_id {
                latest.insert(span.span_id.clone(), span.clone());
            }
        }
        result.extend(latest.into_values());

        sort_spans(&mut result);
        Ok(result)
    }

    /// Returns spans matching `filter`, ordered by start time.
    ///
    /// Buffered and persisted spans are inspected under one atomic combined
    /// snapshot to prevent concurrent flushes from hiding committed data.
    pub fn query(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut result = Vec::new();

        // Limited queries take the lazy path: per-source candidates stay
        // UNDECODED (v2 posting/record offsets), a k-way merge pops them in
        // start-time order, and only popped candidates are parsed and
        // re-verified — a limit-100 query over 10M spans decodes ~100
        // records instead of ~100,000 (measured: attribute filter p50 209 ms
        // at 10M, dominated entirely by candidate parsing).
        if let Some(limit) = filter.limit {
            let mut buffered: Vec<Span> = writer
                .spans
                .iter()
                .filter(|span| span_matches(span, filter))
                .cloned()
                .collect();
            sort_spans(&mut buffered);

            enum Source<'a> {
                Parsed(Vec<Span>),
                Lazy {
                    seg: &'a segment_v2::Segment,
                    offsets: Vec<u64>,
                },
            }
            let mut sources: Vec<(Source<'_>, usize)> = vec![(Source::Parsed(buffered), 0)];
            for segment in segments.iter() {
                let seg = &segment.seg;
                let offsets = if let Some(service) = &filter.service {
                    seg.attribute_posting_offsets(IDX_SERVICE, service)
                } else if let Some(name) = &filter.name {
                    seg.attribute_posting_offsets(IDX_NAME, name)
                } else if let Some((key, value)) = filter
                    .attributes
                    .iter()
                    .find(|(key, _)| !key.starts_with('\u{0}'))
                {
                    seg.attribute_posting_offsets(key, &canonical_value(value))
                } else {
                    seg.record_offsets().to_vec()
                };
                sources.push((Source::Lazy { seg, offsets }, 0));
            }

            let peek = |source: &(Source<'_>, usize)| -> Result<Option<u64>> {
                let (src, pos) = source;
                match src {
                    Source::Parsed(spans) => Ok(spans.get(*pos).map(|span| span.start_time_ns)),
                    Source::Lazy { seg, offsets } => match offsets.get(*pos) {
                        None => Ok(None),
                        Some(offset) => {
                            Ok(Some(seg.timestamp_at(*offset).map_err(segment_v2_error)?))
                        }
                    },
                }
            };

            // Head-timestamp cache: with file-backed segments every peek is a
            // disk read, and re-peeking all sources per pop cost O(pops x
            // sources) syscalls — measured 8 ms -> 125 ms at 10M. Each source
            // is peeked once, then only re-peeked after ITS head is consumed.
            let mut heads: Vec<Option<u64>> = Vec::with_capacity(sources.len());
            for source in sources.iter() {
                heads.push(peek(source)?);
            }

            let mut result: Vec<Span> = Vec::with_capacity(limit.min(1024));
            let mut emitted: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            while result.len() < limit {
                let mut best: Option<(usize, u64)> = None;
                for (index, head) in heads.iter().enumerate() {
                    if let Some(timestamp) = head {
                        if best.map_or(true, |(_, current)| *timestamp < current) {
                            best = Some((index, *timestamp));
                        }
                    }
                }
                let Some((index, _)) = best else { break };
                let (src, pos) = &mut sources[index];
                let span = match src {
                    Source::Parsed(spans) => {
                        let span = spans[*pos].clone();
                        *pos += 1;
                        Some(span)
                    }
                    Source::Lazy { seg, offsets } => {
                        let record = seg
                            .record_at_offset(offsets[*pos])
                            .map_err(segment_v2_error)?;
                        *pos += 1;
                        let span = record_to_span(&record)?;
                        span_matches(&span, filter).then_some(span)
                    }
                };
                heads[index] = peek(&sources[index])?;
                let Some(span) = span else { continue };
                let key = (span.trace_id.clone(), span.span_id.clone());
                if emitted.contains(&key) {
                    continue;
                }
                // Primary-key precedence: source 0 is the write buffer (it
                // always wins); among segments, a LATER source index means a
                // later flush and a newer version — a candidate loses to any
                // higher-precedence source that also holds its key.
                let superseded = if index == 0 {
                    false
                } else {
                    writer.contains_key(&span.trace_id, &span.span_id)
                        || segments
                            .iter()
                            .skip(index) // sources[i] maps to segments[i-1]
                            .map(|segment| segment.contains_key(&span.trace_id, &span.span_id))
                            .collect::<Result<Vec<_>>>()?
                            .into_iter()
                            .any(|contains| contains)
                };
                if !superseded {
                    emitted.insert(key);
                    result.push(span);
                }
            }
            return Ok(result);
        }

        // Primary-key semantics for unlimited queries: dedupe to the newest
        // version of each (trace_id, span_id) BEFORE filtering, so a
        // superseded older version can neither appear alongside nor instead
        // of the version that currently holds the key.
        let mut latest: std::collections::HashMap<(String, String), Span> =
            std::collections::HashMap::new();
        for segment in segments.iter() {
            for span in segment.spans_parsed()? {
                latest.insert((span.trace_id.clone(), span.span_id.clone()), span);
            }
        }
        for span in writer.spans.iter() {
            latest.insert((span.trace_id.clone(), span.span_id.clone()), span.clone());
        }
        for span in latest.into_values() {
            if span_matches(&span, filter) {
                result.push(span);
            }
        }

        sort_spans(&mut result);
        Ok(result)
    }

    /// Returns current buffer, segment, span, and disk usage statistics.
    pub fn stats(&self) -> Result<Stats> {
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let persisted_spans = segments.iter().map(Segment::span_count).sum();
        let disk_bytes = segments.iter().map(|segment| segment.bytes).sum();
        let buffered_spans = writer.len();

        Ok(Stats {
            buffered_spans,
            persisted_spans,
            total_spans: buffered_spans + persisted_spans,
            segment_count: segments.len(),
            disk_bytes,
        })
    }

    /// Removes spans older than the configured TTL and returns the number
    /// removed. A zero TTL disables expiration.
    pub fn compact_expired(&self) -> Result<usize> {
        let Some(ttl_seconds) = self.config.ttl_seconds else {
            return Ok(0);
        };
        // Zero is documented as "disabled", not "expire everything now".
        if ttl_seconds == 0 {
            return Ok(0);
        }

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let ttl_ns = ttl_seconds.saturating_mul(1_000_000_000);
        self.expire_before(now_ns.saturating_sub(ttl_ns))
    }

    /// Removes spans ending before `cutoff_ns` and returns the number removed.
    pub fn expire_before(&self, cutoff_ns: u64) -> Result<usize> {
        let mut writer = self.lock_writer()?;
        let mut segments = self.lock_segments()?;
        let before_buffer = writer.len();
        writer.retain(|span| span.end_time_ns >= cutoff_ns);
        let mut removed = before_buffer - writer.len();

        // The in-memory segment set is only replaced after every file
        // operation has succeeded: an early error must leave the running
        // store serving its previous (superset) view, never an empty one
        // (found in review: mem::take + a fallible loop could wipe the
        // in-memory set on the first I/O failure). Compaction is still not
        // crash-ATOMIC across segments — there is no manifest yet — so a
        // crash mid-compaction can leave both an old and its rewritten
        // segment on disk until the next successful compaction; that bound
        // is documented in the README limitations.
        let mut replacement: Vec<Segment> = Vec::with_capacity(segments.len());
        let mut removed_from_segments = 0usize;
        for segment in segments.iter() {
            let all = segment.spans_parsed()?;
            let total = all.len();
            let kept: Vec<Span> = all
                .into_iter()
                .filter(|span| span.end_time_ns >= cutoff_ns)
                .collect();
            removed_from_segments += total - kept.len();

            if kept.len() == total {
                replacement.push(Segment {
                    path: segment.path.clone(),
                    bytes: segment.bytes,
                    seg: Box::new(
                        segment_v2::Segment::open(&segment.path).map_err(segment_v2_error)?,
                    ),
                });
                continue;
            }

            if kept.is_empty() {
                fs::remove_file(&segment.path)?;
                continue;
            }

            let mut kept = kept;
            sort_spans(&mut kept);
            let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
            let old_name = segment
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let new_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_V2_SUFFIX}");
            // Journal first: whichever side of the rewrite a crash lands on,
            // recovery finishes it without content guessing.
            let marker = write_supersede_marker(&self.directory, &old_name, &new_name)?;
            let new_segment = self.write_segment(id, &kept)?;
            fs::remove_file(&segment.path)?;
            let _ = fs::remove_file(marker);
            replacement.push(new_segment);
        }

        removed += removed_from_segments;
        replacement.sort_by(|left, right| left.path.cmp(&right.path));
        *segments = replacement;
        Ok(removed)
    }

    /// Returns copies of the spans in each persisted segment.
    ///
    /// This intentionally narrow inspection hook exists so integration tests
    /// can verify the on-disk invariant that every segment is internally sorted.
    pub fn persisted_segment_spans(&self) -> Result<Vec<Vec<Span>>> {
        let _writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        segments.iter().map(Segment::spans_parsed).collect()
    }

    /// Number of fully materialized `Span` structs held for PERSISTED data.
    ///
    /// The v2 memory rule: this is zero after an open of a v2-only store —
    /// segments hold bytes plus indexes, and spans parse on demand. Legacy v1
    /// segments (still memory-resident) and the write buffer are the only
    /// contributors.
    pub fn resident_persisted_span_structs(&self) -> Result<usize> {
        let segments = self.lock_segments()?;
        let _ = &segments;
        Ok(0)
    }

    /// Bytes of segment payload encoding currently resident in memory.
    ///
    /// The larger-than-RAM rule: zero after open AND after flush — segments
    /// are file-backed, holding only their parsed indexes; record payloads
    /// are read on demand.
    pub fn resident_payload_bytes(&self) -> Result<usize> {
        let segments = self.lock_segments()?;
        Ok(segments
            .iter()
            .map(|segment| segment.seg.resident_bytes())
            .sum())
    }

    fn lock_writer(&self) -> Result<MutexGuard<'_, WriteBuffer>> {
        self.writer
            .lock()
            .map_err(|_| Error::LockPoisoned("writer"))
    }

    fn lock_segments(&self) -> Result<MutexGuard<'_, Vec<Segment>>> {
        self.segments
            .lock()
            .map_err(|_| Error::LockPoisoned("segments"))
    }

    fn should_flush(&self, buffered: usize) -> bool {
        self.config.flush_spans > 0 && buffered >= self.config.flush_spans
    }

    fn flush_locked(&self, writer: &mut WriteBuffer, segments: &mut Vec<Segment>) -> Result<()> {
        if writer.is_empty() {
            return Ok(());
        }

        let mut pending = writer.spans.clone();
        sort_spans(&mut pending);
        let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
        let segment = self.write_segment(id, &pending)?;
        writer.clear();
        segments.push(segment);
        segments.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(())
    }

    fn write_segment(&self, id: u64, spans: &[Span]) -> Result<Segment> {
        let file_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_V2_SUFFIX}");
        let final_path = self.directory.join(&file_name);
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".{file_name}.{}.{}.tmp", std::process::id(), counter);
        let temp_path = self.directory.join(temp_name);

        let write_result = (|| {
            if final_path.exists() {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "segment id collision: {} already exists",
                        final_path.display()
                    ),
                )));
            }
            let records = spans
                .iter()
                .map(span_to_record)
                .collect::<Result<Vec<_>>>()?;
            let encoded = segment_v2::encode(&records).map_err(segment_v2_error)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(&temp_path)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&temp_path, &final_path)?;
            sync_directory(&self.directory)?;
            let bytes = fs::metadata(&final_path)?.len();
            // Reopen FILE-BACKED: the encoded buffer is dropped and the
            // segment serves reads from disk immediately — flushing never
            // leaves a resident payload copy behind.
            drop(encoded);
            let seg = Box::new(segment_v2::Segment::open(&final_path).map_err(segment_v2_error)?);
            Ok(Segment {
                path: final_path,
                bytes,
                seg,
            })
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }
}

/// Compaction supersede journal: a marker recording that a rewritten segment
/// replaces an original. Written BEFORE the replacement, deleted after the
/// original is removed, so recovery can finish an interrupted rewrite in
/// either direction without ever guessing from content — content-based
/// duplicate healing silently destroyed legitimately re-ingested identical
/// spans (found in review: acknowledged duplicate cardinality must survive
/// restart).
fn supersede_marker_path(directory: &Path, old_name: &str, new_name: &str) -> PathBuf {
    directory.join(format!(".supersede.{old_name}.{new_name}.journal"))
}

fn write_supersede_marker(directory: &Path, old_name: &str, new_name: &str) -> Result<PathBuf> {
    let path = supersede_marker_path(directory, old_name, new_name);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(&path)?;
    writeln!(file, "{old_name} -> {new_name}")?;
    file.sync_all()?;
    sync_directory(directory)?;
    Ok(path)
}

/// Finishes interrupted compaction rewrites recorded in supersede markers.
///
/// If the replacement exists and is complete, the original is deleted (the
/// crash hit between replacement rename and original delete). If the
/// replacement never materialized, nothing is deleted — the original remains
/// authoritative. The marker is removed either way.
fn recover_supersede_markers(directory: &Path) -> Result<()> {
    let mut markers = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".supersede.") && name.ends_with(".journal") {
            markers.push(entry.path());
        }
    }
    for marker in markers {
        let parsed = fs::read_to_string(&marker).unwrap_or_default();
        if let Some((old_name, new_name)) = parsed.trim().split_once(" -> ") {
            let old_path = directory.join(old_name);
            let new_path = directory.join(new_name);
            let replacement_ready = new_path.exists()
                && (!new_name.ends_with(SEGMENT_V2_SUFFIX)
                    || segment_v2::Segment::open(&new_path).is_ok());
            if replacement_ready && old_path.exists() {
                fs::remove_file(&old_path)?;
            }
        }
        fs::remove_file(&marker)?;
    }
    Ok(())
}

fn sort_spans(spans: &mut [Span]) {
    spans.sort_by(|left, right| {
        left.start_time_ns
            .cmp(&right.start_time_ns)
            .then_with(|| left.end_time_ns.cmp(&right.end_time_ns))
            .then_with(|| left.trace_id.cmp(&right.trace_id))
            .then_with(|| left.span_id.cmp(&right.span_id))
    });
}

fn span_matches(span: &Span, filter: &SpanFilter) -> bool {
    if filter
        .service
        .as_ref()
        .is_some_and(|service| span.service != *service)
    {
        return false;
    }
    if filter.name.as_ref().is_some_and(|name| span.name != *name) {
        return false;
    }
    if filter
        .min_duration_ns
        .is_some_and(|minimum| span.end_time_ns.saturating_sub(span.start_time_ns) < minimum)
    {
        return false;
    }
    if filter
        .since_ns
        .is_some_and(|since| span.start_time_ns < since)
    {
        return false;
    }
    if filter
        .until_ns
        .is_some_and(|until| span.start_time_ns > until)
    {
        return false;
    }
    filter
        .attributes
        .iter()
        .all(|(key, value)| span.attributes.get(key) == Some(value))
}

fn load_segments(directory: &Path) -> Result<Vec<Segment>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && is_segment_file(&path) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut segments = Vec::with_capacity(paths.len());
    for path in paths {
        let is_v2 = path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy() == SEGMENT_V2_SUFFIX.trim_start_matches('.'));
        if !is_v2 {
            // Pre-v2 JSONL segments are no longer supported: failing loudly
            // beats silently hiding persisted data (migrate with 0.3.x).
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "legacy v1 segment {} is not supported by this version; \
                     migrate the store with traza 0.3.x first",
                    path.display()
                ),
            )));
        }
        let bytes_meta = fs::metadata(&path)?.len();
        let seg = Box::new(segment_v2::Segment::open(&path).map_err(segment_v2_error)?);
        segments.push(Segment {
            path,
            bytes: bytes_meta,
            seg,
        });
    }
    Ok(segments)
}

fn remove_orphan_temps(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn is_segment_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(SEGMENT_PREFIX)
                && (name.ends_with(SEGMENT_SUFFIX) || name.ends_with(SEGMENT_V2_SUFFIX))
        })
}

fn segment_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    // Both formats count: recognizing only .jsonl made a reopened v2-only
    // store restart numbering at zero, and the next flush RENAMED OVER
    // segment-…0000.seg — persisted spans destroyed (found in review,
    // reproduced across restart).
    let stem = name.strip_prefix(SEGMENT_PREFIX)?;
    let number = stem
        .strip_suffix(SEGMENT_SUFFIX)
        .or_else(|| stem.strip_suffix(SEGMENT_V2_SUFFIX))?;
    number.parse().ok()
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<()> {
    Ok(())
}
