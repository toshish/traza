#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A small durable storage engine for tracing spans.
//!
//! Spans are buffered in memory and periodically persisted as sorted JSON-lines
//! segment files. Reads combine the buffered and persisted data.

pub mod expiration;
pub mod segment_v2;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
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

/// A single tracing span.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Span {
    /// Identifier shared by every span in a trace.
    pub trace_id: String,
    /// Identifier unique to this span within its trace.
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "storage I/O error: {error}"),
            Self::Json(error) => write!(f, "storage JSON error: {error}"),
            Self::AlreadyOpen => write!(f, "store is already open by another writer"),
            Self::LockPoisoned(name) => write!(f, "storage lock poisoned: {name}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::AlreadyOpen | Self::LockPoisoned(_) => None,
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

#[derive(Debug)]
struct Segment {
    path: PathBuf,
    bytes: u64,
    data: SegmentData,
}

/// v1 segments stay fully parsed in memory (legacy); v2 segments hold raw
/// bytes plus their embedded indexes and parse spans on demand — the memory
/// rule of the v2 format: resident cost is bytes + indexes, never Span
/// structs.
#[derive(Debug)]
enum SegmentData {
    V1 { spans: Vec<Span> },
    V2 { seg: Box<segment_v2::Segment> },
}

fn canonical_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn span_to_record(span: &Span) -> Result<segment_v2::RecordInput> {
    let mut attributes = std::collections::BTreeMap::new();
    attributes.insert(IDX_SERVICE.to_owned(), span.service.clone());
    attributes.insert(IDX_NAME.to_owned(), span.name.clone());
    for (key, value) in &span.attributes {
        attributes.insert(key.clone(), canonical_value(value));
    }
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
        match &self.data {
            SegmentData::V1 { spans } => spans.len(),
            SegmentData::V2 { seg } => seg.len(),
        }
    }

    /// Full parse — the rewrite/heal/inspection path, never the query path.
    fn spans_parsed(&self) -> Result<Vec<Span>> {
        match &self.data {
            SegmentData::V1 { spans } => Ok(spans.clone()),
            SegmentData::V2 { seg } => {
                let mut spans = Vec::with_capacity(seg.len());
                for ordinal in 0..seg.len() {
                    if let Some(record) = seg.record(ordinal).map_err(segment_v2_error)? {
                        spans.push(record_to_span(&record)?);
                    }
                }
                Ok(spans)
            }
        }
    }

    fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>> {
        match &self.data {
            SegmentData::V1 { spans } => Ok(spans
                .iter()
                .filter(|span| span.trace_id == trace_id)
                .cloned()
                .collect()),
            SegmentData::V2 { seg } => {
                let records = seg.query_trace(trace_id).map_err(segment_v2_error)?;
                records.iter().map(record_to_span).collect()
            }
        }
    }

    /// Index-accelerated filter: the most selective available index narrows
    /// candidates, then every predicate is re-verified on the parsed span —
    /// an index accelerates a filter, it never changes its semantics.
    fn filter_spans(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        match &self.data {
            SegmentData::V1 { spans } => Ok(spans
                .iter()
                .filter(|span| span_matches(span, filter))
                .cloned()
                .collect()),
            SegmentData::V2 { seg } => {
                let candidates = if let Some(service) = &filter.service {
                    Some(
                        seg.query_attribute(IDX_SERVICE, service)
                            .map_err(segment_v2_error)?,
                    )
                } else if let Some(name) = &filter.name {
                    Some(
                        seg.query_attribute(IDX_NAME, name)
                            .map_err(segment_v2_error)?,
                    )
                } else if let Some((key, value)) = filter.attributes.first() {
                    Some(
                        seg.query_attribute(key, &canonical_value(value))
                            .map_err(segment_v2_error)?,
                    )
                } else if filter.since_ns.is_some() || filter.until_ns.is_some() {
                    Some(
                        seg.query_time_range(
                            filter.since_ns.unwrap_or(0),
                            filter.until_ns.unwrap_or(u64::MAX),
                        )
                        .map_err(segment_v2_error)?,
                    )
                } else {
                    None
                };
                let mut result = Vec::new();
                match candidates {
                    Some(records) => {
                        for record in &records {
                            let span = record_to_span(record)?;
                            if span_matches(&span, filter) {
                                result.push(span);
                            }
                        }
                    }
                    None => {
                        for ordinal in 0..seg.len() {
                            if let Some(record) = seg.record(ordinal).map_err(segment_v2_error)? {
                                let span = record_to_span(&record)?;
                                if span_matches(&span, filter) {
                                    result.push(span);
                                }
                            }
                        }
                    }
                }
                Ok(result)
            }
        }
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
    writer: Mutex<Vec<Span>>,
    segments: Mutex<Vec<Segment>>,
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
            let mut segments = load_segments(&directory)?;
            segments.sort_by(|left, right| left.path.cmp(&right.path));
            // A crash between writing a compacted segment and deleting its
            // original leaves the surviving spans in BOTH files. Recovery
            // heals it here: exact duplicates (identical on every field) are
            // dropped in path order, and a segment whose spans are all
            // duplicates is deleted outright. Later compactions cannot be
            // relied on for this — they only touch expired spans (found in
            // review, with a reproducing crash simulation).
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut healed: Vec<Segment> = Vec::with_capacity(segments.len());
            for segment in segments {
                let spans = segment.spans_parsed()?;
                let kept: Vec<Span> = spans
                    .into_iter()
                    .filter(|span| {
                        serde_json::to_string(span)
                            .map(|canonical| seen.insert(canonical))
                            .unwrap_or(true)
                    })
                    .collect();
                if kept.is_empty() {
                    fs::remove_file(&segment.path)?;
                } else if kept.len() == segment.span_count() {
                    healed.push(segment);
                } else {
                    // Partial duplicates: v1 heals in memory (file untouched,
                    // as before); an immutable v2 file is rewritten in place
                    // under the same id via temp + fsync + rename.
                    match segment.data {
                        SegmentData::V1 { .. } => healed.push(Segment {
                            path: segment.path,
                            bytes: segment.bytes,
                            data: SegmentData::V1 { spans: kept },
                        }),
                        SegmentData::V2 { .. } => {
                            healed.push(rewrite_v2_in_place(&segment.path, &kept)?);
                        }
                    }
                }
            }
            let segments = healed;
            let next_segment = segments
                .iter()
                .filter_map(|segment| segment_number(&segment.path))
                .max()
                .map_or(0, |number| number.saturating_add(1));

            Ok(Self {
                directory,
                config,
                writer: Mutex::new(Vec::new()),
                segments: Mutex::new(segments),
                next_segment: AtomicU64::new(next_segment),
                _directory_lock: directory_lock,
            })
        })();

        opened
    }

    /// Adds one span, automatically flushing when the configured threshold is
    /// reached.
    pub fn ingest(&self, span: Span) -> Result<()> {
        let mut writer = self.lock_writer()?;
        writer.push(span);
        if self.should_flush(writer.len()) {
            let mut segments = self.lock_segments()?;
            self.flush_locked(&mut writer, &mut segments)?;
        }
        Ok(())
    }

    /// Adds a batch of spans, automatically flushing when the configured
    /// threshold is reached.
    pub fn ingest_batch(&self, spans: Vec<Span>) -> Result<()> {
        if spans.is_empty() {
            return Ok(());
        }

        let mut writer = self.lock_writer()?;
        writer.extend(spans);
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

        for span in writer.iter() {
            if span.trace_id == trace_id {
                result.push(span.clone());
            }
        }
        for segment in segments.iter() {
            result.extend(segment.trace_spans(trace_id)?);
        }

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

        for span in writer.iter() {
            if span_matches(span, filter) {
                result.push(span.clone());
            }
        }
        for segment in segments.iter() {
            result.extend(segment.filter_spans(filter)?);
        }

        sort_spans(&mut result);
        if let Some(limit) = filter.limit {
            result.truncate(limit);
        }
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
                replacement.push(match &segment.data {
                    SegmentData::V1 { .. } => Segment {
                        path: segment.path.clone(),
                        bytes: segment.bytes,
                        data: SegmentData::V1 { spans: kept },
                    },
                    SegmentData::V2 { .. } => Segment {
                        path: segment.path.clone(),
                        bytes: segment.bytes,
                        data: SegmentData::V2 {
                            seg: Box::new(
                                segment_v2::Segment::open(&segment.path)
                                    .map_err(segment_v2_error)?,
                            ),
                        },
                    },
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
            let new_segment = self.write_segment(id, &kept)?;
            fs::remove_file(&segment.path)?;
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
        Ok(segments
            .iter()
            .map(|segment| match &segment.data {
                SegmentData::V1 { spans } => spans.len(),
                SegmentData::V2 { .. } => 0,
            })
            .sum())
    }

    fn lock_writer(&self) -> Result<MutexGuard<'_, Vec<Span>>> {
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

    fn flush_locked(&self, writer: &mut Vec<Span>, segments: &mut Vec<Segment>) -> Result<()> {
        if writer.is_empty() {
            return Ok(());
        }

        let mut pending = writer.clone();
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
            let seg = Box::new(segment_v2::Segment::from_bytes(encoded).map_err(segment_v2_error)?);
            Ok(Segment {
                path: final_path,
                bytes,
                data: SegmentData::V2 { seg },
            })
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }
}

/// Rewrites a v2 segment file in place (same id) via temp + fsync + rename.
fn rewrite_v2_in_place(path: &Path, spans: &[Span]) -> Result<Segment> {
    let records = spans
        .iter()
        .map(span_to_record)
        .collect::<Result<Vec<_>>>()?;
    let encoded = segment_v2::encode(&records).map_err(segment_v2_error)?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        counter
    ));
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        let bytes = fs::metadata(path)?.len();
        let seg = Box::new(segment_v2::Segment::from_bytes(encoded).map_err(segment_v2_error)?);
        Ok(Segment {
            path: path.to_path_buf(),
            bytes,
            data: SegmentData::V2 { seg },
        })
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
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
        let bytes_meta = fs::metadata(&path)?.len();
        let is_v2 = path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy() == SEGMENT_V2_SUFFIX.trim_start_matches('.'));
        if is_v2 {
            let seg = Box::new(segment_v2::Segment::open(&path).map_err(segment_v2_error)?);
            segments.push(Segment {
                path,
                bytes: bytes_meta,
                data: SegmentData::V2 { seg },
            });
        } else {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut spans = Vec::new();
            for line in reader.lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    spans.push(serde_json::from_str(&line)?);
                }
            }
            sort_spans(&mut spans);
            segments.push(Segment {
                path,
                bytes: bytes_meta,
                data: SegmentData::V1 { spans },
            });
        }
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
    let number = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
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
