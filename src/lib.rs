#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A small durable storage engine for tracing spans.
//!
//! Spans are buffered in memory and periodically persisted as sorted,
//! file-backed indexed segments. Reads combine the buffered and persisted data.

pub mod analytics;
pub mod annotations;
pub mod auth;
pub mod expiration;
mod media;
pub mod metrics;
pub mod otlp;
pub mod otlp_pb;
pub mod payload;
pub mod seed;
pub mod segment;
pub mod semconv;
pub mod ui;
mod wal;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Nanoseconds since `started`, saturated into a `u64` (~584 years).
fn elapsed_nanos(started: &Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

const LOCK_FILE_NAME: &str = "LOCK";
const SEGMENT_PREFIX: &str = "segment-";
/// Suffix of the unsupported legacy (v1 JSONL) segment files, recognized
/// only so they can be rejected with a migration pointer.
const LEGACY_SEGMENT_SUFFIX: &str = ".jsonl";
/// Suffix for the current indexed segment files.
const SEGMENT_SUFFIX: &str = ".seg";
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
    /// Match spans belonging to this session, resolved across every recognized
    /// session key (`session.id`, `gen_ai.conversation.id`, a
    /// `traceloop.association.properties.*` key). Unlike an `attr.KEY` filter,
    /// this unions the recognized keys, so a session whose spans use mixed
    /// conventions is returned whole (see [`crate::semconv`]).
    pub session: Option<String>,
    /// Maximum number of returned spans.
    pub limit: Option<usize>,
}

/// Exclusive position in Traza's stable span order.
///
/// Passing a cursor to [`Store::query_after`] returns only spans ordered after
/// `(start_time_ns, end_time_ns, trace_id, span_id)`. This is the bounded
/// pagination primitive used by dataset export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanCursor {
    /// Span start timestamp.
    pub start_time_ns: u64,
    /// Span end timestamp.
    pub end_time_ns: u64,
    /// Trace identifier.
    pub trace_id: String,
    /// Span identifier.
    pub span_id: String,
}

impl From<&Span> for SpanCursor {
    fn from(span: &Span) -> Self {
        Self {
            start_time_ns: span.start_time_ns,
            end_time_ns: span.end_time_ns,
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
        }
    }
}

/// What an acknowledged write guarantees.
///
/// The mode is the store's contract with its clients, so it is chosen per
/// deployment and reported rather than inferred. Ordering of strength:
/// `Buffered` < `Wal` < `Flushed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// Acknowledged means accepted in memory. A crash loses everything not
    /// yet sealed into a segment. Fast, and **lossy by design** — appropriate
    /// for laptops, CI, and benchmarks, not for production.
    Buffered,
    /// Acknowledged means fsynced to the write-ahead log and recoverable on
    /// restart. The production default: durability without paying a segment
    /// write per request, because group commit amortizes the fsync.
    #[default]
    Wal,
    /// Acknowledged means present in a sealed segment. The strongest and
    /// slowest mode — every ingest call seals a segment.
    Flushed,
}

impl Durability {
    /// Parses the wire/CLI name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "buffered" => Some(Self::Buffered),
            "wal" => Some(Self::Wal),
            "flushed" => Some(Self::Flushed),
            _ => None,
        }
    }

    /// The wire/CLI name, as reported to clients.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buffered => "buffered",
            Self::Wal => "wal",
            Self::Flushed => "flushed",
        }
    }
}

/// Size-tiered compaction settings.
///
/// Segment count, not corpus size, is what filtered search pays for: a
/// query narrows candidates through each segment's index, so latency grows
/// with the number of segments. A store that only ever appends flush-sized
/// segments therefore gets steadily slower to search. Compaction bounds that
/// count by merging segments of similar size into larger ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Segments of the same size tier that trigger a merge. Larger values
    /// merge less often but leave more segments to search.
    pub fanout: usize,
    /// Size ceiling for tier 0. Segments smaller than this are all "tier 0",
    /// and each subsequent tier is `fanout` times larger.
    pub base_bytes: u64,
    /// Never merge into a segment larger than this. Bounds both the memory a
    /// merge needs (it materializes its inputs) and how long the segment lock
    /// is held; the cost is a floor on how far the segment count can fall.
    pub max_segment_bytes: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            fanout: 4,
            base_bytes: 8 * 1024 * 1024,
            max_segment_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Storage configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Number of buffered spans that triggers an automatic flush.
    pub flush_spans: usize,
    /// Retention period in seconds; zero disables TTL expiration.
    pub ttl_seconds: Option<u64>,
    /// String attribute values longer than this many bytes are offloaded to
    /// the content-addressed payload store and replaced by a reference
    /// object (see [`payload`]). `None` disables offloading.
    pub payload_threshold: Option<usize>,
    /// What an acknowledged ingest guarantees. Defaults to [`Durability::Wal`]:
    /// a store that silently loses acknowledged writes is the wrong default,
    /// even though it is the faster one.
    pub durability: Durability,
    /// Size-tiered compaction, or `None` to leave segments as flushed.
    /// Enabled by default: without it, filtered-search latency grows without
    /// bound as segments accumulate.
    pub compaction: Option<CompactionConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            flush_spans: 10_000,
            ttl_seconds: None,
            payload_threshold: None,
            durability: Durability::Wal,
            compaction: Some(CompactionConfig::default()),
        }
    }
}

/// A point-in-time summary of store usage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Number of primary-key-unique records currently buffered in memory.
    pub buffered_records: usize,
    /// Number of physical records in persisted segments, including historical
    /// versions superseded by last-write-wins reads.
    pub persisted_records: usize,
    /// Total physical buffered and persisted records.
    pub total_records: usize,
    /// Number of persisted segment files.
    pub segment_count: usize,
    /// Total size of persisted segment files in bytes.
    pub disk_bytes: u64,
    /// What an acknowledged write currently guarantees.
    pub durability: Durability,
    /// Bytes the write-ahead log holds, i.e. the work a restart would replay.
    /// Zero in [`Durability::Buffered`], and immediately after a flush.
    pub wal_bytes: u64,
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

/// A persisted file-backed segment plus its embedded indexes. Span payloads
/// parse on demand and do not remain resident.
#[derive(Debug)]
struct Segment {
    path: PathBuf,
    bytes: u64,
    seg: Box<segment::Segment>,
}

fn canonical_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn span_to_record(span: &Span) -> Result<segment::RecordInput> {
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
    Ok(segment::RecordInput::new(
        span.start_time_ns,
        span.trace_id.clone(),
        attributes,
        serde_json::to_vec(span)?,
    ))
}

fn record_to_span(record: &segment::Record) -> Result<Span> {
    Ok(serde_json::from_slice(record.payload())?)
}

impl Segment {
    fn record_count(&self) -> usize {
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
            if let Some(record) = self.seg.record(ordinal).map_err(segment_error)? {
                spans.push(record_to_span(&record)?);
            }
        }
        Ok(spans)
    }

    fn trace_spans(&self, trace_id: &str) -> Result<Vec<Span>> {
        let records = self.seg.query_trace(trace_id).map_err(segment_error)?;
        records.iter().map(record_to_span).collect()
    }
}

fn segment_error(error: segment::Error) -> Error {
    match error {
        segment::Error::Io(inner) => Error::Io(inner),
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

    /// Reinstates spans taken out of the buffer, rebuilding the position index.
    ///
    /// The index maps a primary key to a POSITION, so handing back a reordered
    /// vector without rebuilding it would leave `upsert` overwriting the wrong
    /// span. Used only on the failed-seal path, which sorts before it writes.
    fn restore(&mut self, spans: Vec<Span>) {
        self.spans = spans;
        self.index.clear();
        for (position, span) in self.spans.iter().enumerate() {
            self.index
                .insert((span.trace_id.clone(), span.span_id.clone()), position);
        }
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
    recent_payloads: payload::TouchRegistry,
    annotations: annotations::AnnotationLog,
    next_segment: AtomicU64,
    /// Present unless durability is [`Durability::Buffered`]. Guards the gap
    /// between acknowledging a write and sealing it into a segment.
    wal: Option<wal::Wal>,
    metrics: metrics::Metrics,
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

            // Replay BEFORE accepting new writes. Records are append-ordered
            // and upserted in that order, so the newest version of a
            // re-ingested key wins exactly as it did before the crash.
            let mut buffer = WriteBuffer::default();
            let wal = if config.durability == Durability::Buffered {
                // A buffered store makes no durability promise, so it neither
                // writes nor reads a log. An existing log is left untouched:
                // restarting in wal mode must still recover it.
                None
            } else {
                for span in wal::Wal::replay(&directory)? {
                    buffer.upsert(span);
                }
                Some(wal::Wal::open(&directory)?)
            };
            let next_segment = segments
                .iter()
                .filter_map(|segment| segment_number(&segment.path))
                .max()
                .map_or(0, |number| number.saturating_add(1));

            Ok(Self {
                annotations: annotations::AnnotationLog::open(&directory)?,
                directory,
                config,
                writer: Mutex::new(buffer),
                segments: Mutex::new(segments),
                rollups: Mutex::new(std::collections::HashMap::new()),
                recent_payloads: payload::TouchRegistry::default(),
                next_segment: AtomicU64::new(next_segment),
                wal,
                metrics: metrics::Metrics::default(),
                _directory_lock: directory_lock,
            })
        })();

        opened
    }

    /// Adds one span, automatically flushing when the configured threshold is
    /// reached.
    pub fn ingest(&self, mut span: Span) -> Result<()> {
        validate_span(&span)?;
        if let Some(threshold) = self.config.payload_threshold {
            payload::offload_span(&self.directory, &mut span, threshold, &self.recent_payloads)?;
        }
        self.admit(vec![span])
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
        let mut spans = spans;
        if let Some(threshold) = self.config.payload_threshold {
            for span in &mut spans {
                payload::offload_span(&self.directory, span, threshold, &self.recent_payloads)?;
            }
        }

        self.admit(spans)
    }

    /// The acknowledgement path shared by both ingest surfaces.
    ///
    /// Ordering is the whole contract:
    /// 1. append the batch to the log and upsert it into the buffer, both
    ///    under the writer lock, so a concurrent flush cannot seal a buffer
    ///    that disagrees with the log;
    /// 2. release the lock;
    /// 3. fsync, and only then return.
    ///
    /// The fsync deliberately happens OUTSIDE the lock: that is what lets
    /// concurrent batches accumulate into one sync instead of serializing an
    /// fsync per request. A crash before step 3 loses the batch, which is
    /// correct — nothing was acknowledged yet.
    fn admit(&self, spans: Vec<Span>) -> Result<()> {
        // Encode the log frame BEFORE taking the writer lock. Serializing a
        // batch is pure CPU proportional to its size, and doing it under the
        // lock made every concurrent ingest wait for it — the lock was held
        // for the serialization of every batch in the system, one at a time.
        // Only the file write has to be inside the lock (below).
        let frame = match &self.wal {
            Some(_) => Some(self.metrics.wal_encode.time(|| wal::Wal::encode(&spans))?),
            None => None,
        };
        let admitted = spans.len() as u64;

        let mut pending_commit = None;
        {
            let waited = Instant::now();
            let mut writer = self.lock_writer()?;
            self.metrics.writer_lock_wait.record(elapsed_nanos(&waited));
            if let (Some(log), Some(frame)) = (&self.wal, &frame) {
                pending_commit = Some(self.metrics.wal_write.time(|| log.append(frame))?);
            }
            self.metrics.buffer_upsert.time(|| {
                for span in spans {
                    writer.upsert(span);
                }
            });
            if self.should_flush(writer.len()) {
                let mut segments = self.lock_segments()?;
                // A sealed segment supersedes the log, so this also discards
                // it and satisfies any commit still waiting on it.
                self.flush_locked(&mut writer, &mut segments)?;
                pending_commit = None;
            }
        }
        if let (Some(log), Some(lsn)) = (&self.wal, pending_commit) {
            log.commit(lsn, &self.metrics)?;
        }
        self.metrics.spans_admitted.add(admitted);
        self.metrics.batches_admitted.increment();
        Ok(())
    }

    /// Per-stage ingest instrumentation. See [`metrics::Metrics`].
    pub fn metrics(&self) -> &metrics::Metrics {
        &self.metrics
    }

    /// What an acknowledged ingest currently guarantees.
    pub fn durability(&self) -> Durability {
        self.config.durability
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

    /// Returns spans matching `filter`, ordered by Traza's stable span order.
    ///
    /// Buffered and persisted spans are inspected under one atomic combined
    /// snapshot to prevent concurrent flushes from hiding committed data.
    pub fn query(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        self.query_after(filter, None)
    }

    /// Every span carrying `values` under ANY of `keys`, resolved under ONE
    /// snapshot of the write buffer and segment list.
    ///
    /// The snapshot matters: resolving each key with its own [`Self::query`]
    /// call let a span re-ingested between the calls be seen first in its
    /// SUPERSEDED version, which the per-key dedupe then locked in — the newer
    /// version arrived later under the same primary key and was discarded.
    /// That broke last-write-wins during ordinary concurrent ingest. Holding
    /// both locks across every key makes the union as atomic as a single
    /// query, and precedence is the usual one: the write buffer wins, then the
    /// newest segment.
    ///
    /// `values` lists the accepted encodings of one logical value (a session
    /// id may arrive as the string `"42"` or the number `42`), so the caller
    /// does not have to guess which JSON type a producer used.
    pub(crate) fn query_attribute_union(
        &self,
        keys: &[&str],
        values: &[Value],
    ) -> Result<Vec<Span>> {
        // Lock order: writer before segments (see Store field docs).
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let matches = |span: &Span| {
            keys.iter().any(|key| {
                span.attributes
                    .get(*key)
                    .is_some_and(|held| values.iter().any(|value| held == value))
            })
        };

        let mut result: Vec<Span> = Vec::new();
        let mut claimed: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // The buffer holds the newest version of anything it carries.
        for span in writer.spans.iter() {
            if matches(span) {
                claimed.insert((span.trace_id.clone(), span.span_id.clone()));
                result.push(span.clone());
            }
        }
        // Any key present in the buffer supersedes every segment copy, even
        // one the predicate does not select in its buffered version.
        for (trace_id, span_id) in writer.index.keys() {
            claimed.insert((trace_id.clone(), span_id.clone()));
        }
        // Newest segment first, so the first version claimed for a key wins.
        for segment in segments.iter().rev() {
            let seg = &segment.seg;
            let mut offsets: Vec<u64> = Vec::new();
            for key in keys {
                for value in values {
                    offsets.extend_from_slice(
                        seg.attribute_posting_offsets_ref(key, &canonical_value(value)),
                    );
                }
            }
            offsets.sort_unstable();
            offsets.dedup();
            for offset in offsets {
                let record = seg.record_at_offset(offset).map_err(segment_error)?;
                let span = record_to_span(&record)?;
                // An index accelerates a filter, it never changes it.
                if !matches(&span) {
                    continue;
                }
                if claimed.insert((span.trace_id.clone(), span.span_id.clone())) {
                    result.push(span);
                }
            }
        }
        sort_spans(&mut result);
        Ok(result)
    }

    /// Returns spans matching `filter` strictly after `cursor`.
    ///
    /// The cursor is compared using the same total order as [`Self::query`],
    /// allowing callers to paginate with a constant result bound even when
    /// many spans share one timestamp.
    pub fn query_after(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
    ) -> Result<Vec<Span>> {
        // A session predicate unions candidates across recognized keys, which
        // the single-key attribute index cannot express — resolve it up front,
        // then apply the remaining predicates, order, and limit.
        if let Some(session_id) = &filter.session {
            let mut spans = self.resolve_session_spans(session_id)?;
            spans.retain(|span| {
                span_matches(span, filter)
                    && cursor.map_or(true, |position| span_after_cursor(span, position))
            });
            sort_spans(&mut spans);
            if let Some(limit) = filter.limit {
                spans.truncate(limit);
            }
            return Ok(spans);
        }
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut result = Vec::new();

        // Limited queries take the lazy path: per-source candidates stay as
        // v2 posting/record offsets and a k-way merge decodes one head per
        // source. Heads are compared with the SAME total order used by
        // unlimited queries (start, end, trace, span). Comparing only the
        // timestamp made equal-time ties depend on segment/source order;
        // cursor consumers such as export then skipped valid rows.
        if let Some(limit) = filter.limit {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let mut buffered: Vec<Span> = writer
                .spans
                .iter()
                .filter(|span| {
                    span_matches(span, filter)
                        && cursor.map_or(true, |position| span_after_cursor(span, position))
                })
                .cloned()
                .collect();
            sort_spans(&mut buffered);

            enum Source<'a> {
                Parsed(Vec<Span>),
                Lazy {
                    seg: &'a segment::Segment,
                    offsets: &'a [u64],
                },
            }
            let mut sources: Vec<(Source<'_>, usize)> = vec![(Source::Parsed(buffered), 0)];
            for segment in segments.iter() {
                let seg = &segment.seg;
                let offsets = if let Some(service) = &filter.service {
                    seg.attribute_posting_offsets_ref(IDX_SERVICE, service)
                } else if let Some(name) = &filter.name {
                    seg.attribute_posting_offsets_ref(IDX_NAME, name)
                } else if let Some((key, value)) = filter
                    .attributes
                    .iter()
                    .find(|(key, _)| !key.starts_with('\u{0}'))
                {
                    seg.attribute_posting_offsets_ref(key, &canonical_value(value))
                } else {
                    seg.record_offsets()
                };
                let position = match cursor {
                    Some(cursor) => first_offset_after(seg, offsets, cursor)?,
                    None => 0,
                };
                sources.push((Source::Lazy { seg, offsets }, position));
            }

            let advance = |source: &mut (Source<'_>, usize)| -> Result<Option<Span>> {
                let (src, pos) = source;
                match src {
                    Source::Parsed(spans) => {
                        let span = spans.get(*pos).cloned();
                        if span.is_some() {
                            *pos += 1;
                        }
                        Ok(span)
                    }
                    Source::Lazy { seg, offsets } => {
                        let Some(offset) = offsets.get(*pos).copied() else {
                            return Ok(None);
                        };
                        *pos += 1;
                        let record = seg.record_at_offset(offset).map_err(segment_error)?;
                        record_to_span(&record).map(Some)
                    }
                }
            };

            // Decode and cache one head per source. Only the selected source
            // advances, so each record is read at most once.
            let mut heads: Vec<Option<Span>> = Vec::with_capacity(sources.len());
            for source in sources.iter_mut() {
                heads.push(advance(source)?);
            }

            let mut result: Vec<Span> = Vec::with_capacity(limit.min(1024));
            let mut emitted: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            while result.len() < limit {
                let mut best: Option<usize> = None;
                for (index, head) in heads.iter().enumerate() {
                    if let Some(head) = head {
                        if best.map_or(true, |current| {
                            compare_spans(head, heads[current].as_ref().expect("best head exists"))
                                .is_lt()
                        }) {
                            best = Some(index);
                        }
                    }
                }
                let Some(index) = best else { break };
                let span = heads[index].take().expect("selected head exists");
                heads[index] = advance(&mut sources[index])?;
                if index != 0
                    && (!span_matches(&span, filter)
                        || cursor.is_some_and(|position| !span_after_cursor(&span, position)))
                {
                    continue;
                }
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

        // Unlimited queries narrow candidates through the SAME index the
        // limited path uses. Decoding every record to answer a selective
        // filter made a session lookup on a 50k-span store take 1.3 s, and the
        // cost grew with the store rather than with the answer.
        //
        // Primary-key semantics are preserved without materializing the
        // corpus: a candidate is emitted only if no higher-precedence source
        // (the write buffer, or a newer segment) also holds its key, which is
        // an index lookup rather than a decode. Filtering a candidate before
        // that check is safe — a superseded older version is dropped either
        // way, and the version that currently holds the key is never
        // superseded by definition.
        for (position, segment) in segments.iter().enumerate() {
            let seg = &segment.seg;
            let offsets = if let Some(service) = &filter.service {
                seg.attribute_posting_offsets_ref(IDX_SERVICE, service)
            } else if let Some(name) = &filter.name {
                seg.attribute_posting_offsets_ref(IDX_NAME, name)
            } else if let Some((key, value)) = filter
                .attributes
                .iter()
                .find(|(key, _)| !key.starts_with('\u{0}'))
            {
                seg.attribute_posting_offsets_ref(key, &canonical_value(value))
            } else {
                seg.record_offsets()
            };
            for offset in offsets {
                let record = seg.record_at_offset(*offset).map_err(segment_error)?;
                let span = record_to_span(&record)?;
                if !span_matches(&span, filter)
                    || cursor.is_some_and(|bound| !span_after_cursor(&span, bound))
                {
                    continue;
                }
                if writer.contains_key(&span.trace_id, &span.span_id) {
                    continue; // the buffer holds a newer version
                }
                let mut superseded = false;
                for newer in segments.iter().skip(position + 1) {
                    if newer.contains_key(&span.trace_id, &span.span_id)? {
                        superseded = true;
                        break;
                    }
                }
                if !superseded {
                    result.push(span);
                }
            }
        }
        for span in writer.spans.iter() {
            if span_matches(span, filter)
                && cursor.map_or(true, |bound| span_after_cursor(span, bound))
            {
                result.push(span.clone());
            }
        }

        sort_spans(&mut result);
        Ok(result)
    }

    /// Returns current buffer, segment, physical-record, and disk statistics.
    ///
    /// Persisted counts are physical records rather than logical
    /// last-write-wins cardinality. Naming that distinction explicitly keeps
    /// this operation O(number of segments) instead of decoding the corpus.
    pub fn stats(&self) -> Result<Stats> {
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let persisted_records = segments.iter().map(Segment::record_count).sum();
        let disk_bytes = segments.iter().map(|segment| segment.bytes).sum();
        let buffered_records = writer.len();

        Ok(Stats {
            buffered_records,
            persisted_records,
            total_records: buffered_records.saturating_add(persisted_records),
            segment_count: segments.len(),
            disk_bytes,
            durability: self.config.durability,
            wal_bytes: self.wal.as_ref().map_or(0, wal::Wal::size_bytes),
        })
    }

    /// Removes spans older than the configured TTL and returns the number
    /// removed. A zero TTL disables expiration.
    /// Records one annotation durably (see [`annotations::Annotation`]).
    pub fn annotate(&self, annotation: annotations::Annotation) -> Result<()> {
        self.annotations.append(annotation)
    }

    /// Annotations for a trace, optionally narrowed to one span or name.
    pub fn annotations(
        &self,
        trace_id: &str,
        span_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Vec<annotations::Annotation>> {
        self.annotations.query(trace_id, span_id, name)
    }

    /// Reads an offloaded payload by its `sha256/<hex>` reference.
    pub fn payload(&self, reference: &str) -> Result<Option<Vec<u8>>> {
        payload::load_payload(&self.directory, reference)
    }

    /// Expires spans, annotations, and payload files older than the
    /// configured TTL (no-op when TTL is unset or zero).
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
        let cutoff_ns = now_ns.saturating_sub(ttl_ns);
        let removed = self.expire_before(cutoff_ns)?;
        // Same retention window for the satellite stores: annotations by
        // their own timestamps, payload files by mtime (an orphan payload
        // lingers at most one TTL past its span).
        self.annotations.drop_older_than(cutoff_ns)?;
        let cutoff_time = UNIX_EPOCH + std::time::Duration::from_nanos(cutoff_ns);
        // Live references computed AFTER span expiry, so payloads referenced
        // only by just-expired spans become sweepable.
        let live_refs = self.live_payload_refs()?;
        payload::sweep_expired(
            &self.directory,
            cutoff_time,
            &live_refs,
            &self.recent_payloads,
        )?;
        Ok(removed)
    }

    /// Merges same-size segments so filtered search does not slow down as the
    /// store grows. Returns the number of segments removed by merging.
    ///
    /// **Why segment count matters.** A filtered query narrows candidates
    /// through every segment's index, so its cost is proportional to the
    /// number of segments, not the size of the corpus. A store that only
    /// appends flush-sized segments accumulates them without bound and gets
    /// steadily slower to search. Measured at 10M spans: ~1000 segments gave
    /// an attribute filter p50 of 14.8 ms, against 0.7 ms for the same data
    /// in a single segment.
    ///
    /// **Ordering is the correctness constraint.** Segment path order IS
    /// recency order — `query` resolves the primary key by treating a later
    /// segment as newer. A merged segment takes a fresh (highest) id, so it
    /// lands at the newest position; that is only sound if the run it
    /// replaces was already at the tail. Merging a run from the middle would
    /// promote its spans past segments that legitimately supersede them, so
    /// this only ever compacts the tail.
    ///
    /// **Crash safety** reuses the existing supersede journal, one marker per
    /// input. Recovery deletes an input only once the merged segment is
    /// present and parses; otherwise the inputs stay authoritative and the
    /// merge is simply retried. The merged segment is written and renamed
    /// into place BEFORE any input is deleted, so no window drops data.
    pub fn compact_segments(&self) -> Result<usize> {
        let Some(settings) = self.config.compaction else {
            return Ok(0);
        };
        if settings.fanout < 2 {
            return Ok(0);
        }
        let mut merged_away = 0usize;
        // Each pass merges one run; a merge can create a run one tier up, so
        // loop until nothing qualifies. Bounded by the tier count.
        loop {
            let segments = self.lock_segments()?;
            let Some(run) = tail_run_to_merge(&segments, &settings) else {
                break;
            };
            drop(segments);
            merged_away += self.merge_tail_run(run, &settings)?;
        }
        Ok(merged_away)
    }

    /// Merges the last `run` segments into one. Returns segments removed.
    fn merge_tail_run(&self, run: usize, settings: &CompactionConfig) -> Result<usize> {
        let mut segments = self.lock_segments()?;
        // Re-check under the lock: the set may have changed since the scan.
        if tail_run_to_merge(&segments, settings) != Some(run) {
            return Ok(0);
        }
        let start = segments.len() - run;
        let inputs: Vec<PathBuf> = segments[start..]
            .iter()
            .map(|segment| segment.path.clone())
            .collect();

        // Oldest first, so a later segment's version of a key overwrites an
        // earlier one — the same last-write-wins rule reads apply.
        let mut latest: std::collections::HashMap<(String, String), Span> =
            std::collections::HashMap::new();
        let mut order: Vec<(String, String)> = Vec::new();
        for segment in &segments[start..] {
            for span in segment.spans_parsed()? {
                let key = (span.trace_id.clone(), span.span_id.clone());
                if latest.insert(key.clone(), span).is_none() {
                    order.push(key);
                }
            }
        }
        let mut merged: Vec<Span> = order
            .into_iter()
            .filter_map(|key| latest.remove(&key))
            .collect();
        sort_spans(&mut merged);

        let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
        let new_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}");
        // Journal every input before the replacement exists, so recovery can
        // finish the merge from either side without inspecting content.
        let mut markers = Vec::with_capacity(inputs.len());
        for input in &inputs {
            let old_name = input
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            markers.push(write_supersede_marker(
                &self.directory,
                &old_name,
                &new_name,
            )?);
        }
        let new_segment = self.write_segment(id, &merged)?;
        for input in &inputs {
            fs::remove_file(input)?;
        }
        for marker in markers {
            let _ = fs::remove_file(marker);
        }

        segments.truncate(start);
        segments.push(new_segment);
        segments.sort_by(|left, right| left.path.cmp(&right.path));
        // Rollups are keyed by path; the inputs' entries are now dead.
        if let Ok(mut rollups) = self.rollups.lock() {
            rollups.retain(|path, _| !inputs.contains(path));
        }
        Ok(inputs.len().saturating_sub(1))
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
                    seg: Box::new(segment::Segment::open(&segment.path).map_err(segment_error)?),
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
            let new_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}");
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
    /// The v2 memory rule: this is zero after open and flush. Segments hold
    /// file handles plus indexes, and spans parse on demand.
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
        // `flushed` acknowledges only sealed spans, so every call seals.
        if self.config.durability == Durability::Flushed {
            return buffered > 0;
        }
        self.config.flush_spans > 0 && buffered >= self.config.flush_spans
    }

    fn flush_locked(&self, writer: &mut WriteBuffer, segments: &mut Vec<Segment>) -> Result<()> {
        if writer.is_empty() {
            return Ok(());
        }

        let sealing = Instant::now();
        // Take the spans rather than cloning them: a seal moved the whole
        // buffer (10,000 spans by default) through a deep clone for no reason.
        // They go back if the write fails, so a failed seal still leaves the
        // buffer — and therefore the acknowledged data — intact.
        let mut pending = std::mem::take(&mut writer.spans);
        sort_spans(&mut pending);
        let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
        let segment = match self.write_segment(id, &pending) {
            Ok(segment) => segment,
            Err(error) => {
                writer.restore(pending);
                return Err(error);
            }
        };
        let sealed = pending.len() as u64;
        writer.clear();
        segments.push(segment);
        segments.sort_by(|left, right| left.path.cmp(&right.path));
        // The segment is fsynced and renamed, so every log record it covers is
        // superseded. Reclaim AFTER the segment lands: a crash in between
        // simply replays records the segment already holds, which upsert
        // resolves to the same state.
        if let Some(log) = &self.wal {
            log.reset()?;
        }
        self.metrics.segment_seal.record(elapsed_nanos(&sealing));
        self.metrics.segment_seal_spans.add(sealed);
        Ok(())
    }

    fn write_segment(&self, id: u64, spans: &[Span]) -> Result<Segment> {
        let file_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}");
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
            let encoded = segment::encode(&records).map_err(segment_error)?;
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
            let seg = Box::new(segment::Segment::open(&final_path).map_err(segment_error)?);
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

/// Size tier of a segment: tier 0 is anything below `base_bytes`, and each
/// tier above holds segments up to `fanout` times larger than the last. Two
/// segments in the same tier are within a factor of `fanout` in size, which
/// is what makes merging them worthwhile rather than rewriting a large
/// segment to absorb a tiny one.
fn size_tier(bytes: u64, settings: &CompactionConfig) -> u32 {
    let mut tier = 0u32;
    let mut limit = settings.base_bytes.max(1);
    while bytes >= limit && tier < 32 {
        limit = limit.saturating_mul(settings.fanout.max(2) as u64);
        tier += 1;
    }
    tier
}

/// Length of the maximal same-tier run at the TAIL of `segments`, when that
/// run is long enough to merge and small enough to stay under the size cap.
///
/// Tail-only is a correctness requirement, not a simplification: see
/// [`Store::compact_segments`].
fn tail_run_to_merge(segments: &[Segment], settings: &CompactionConfig) -> Option<usize> {
    if segments.len() < settings.fanout {
        return None;
    }
    let tier = size_tier(segments.last()?.bytes, settings);
    let mut run = 0usize;
    let mut total = 0u64;
    for segment in segments.iter().rev() {
        if size_tier(segment.bytes, settings) != tier {
            break;
        }
        let projected = total.saturating_add(segment.bytes);
        // Stop before exceeding the cap, but only once we already have enough
        // to merge — otherwise a single oversized segment blocks the tier.
        if settings.max_segment_bytes > 0
            && projected > settings.max_segment_bytes
            && run >= settings.fanout
        {
            break;
        }
        total = projected;
        run += 1;
    }
    if run >= settings.fanout
        && !(settings.max_segment_bytes > 0 && total > settings.max_segment_bytes)
    {
        Some(run)
    } else {
        None
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
                && (!new_name.ends_with(SEGMENT_SUFFIX)
                    || segment::Segment::open(&new_path).is_ok());
            if replacement_ready && old_path.exists() {
                fs::remove_file(&old_path)?;
            }
        }
        fs::remove_file(&marker)?;
    }
    Ok(())
}

fn compare_spans(left: &Span, right: &Span) -> std::cmp::Ordering {
    left.start_time_ns
        .cmp(&right.start_time_ns)
        .then_with(|| left.end_time_ns.cmp(&right.end_time_ns))
        .then_with(|| left.trace_id.cmp(&right.trace_id))
        .then_with(|| left.span_id.cmp(&right.span_id))
}

fn compare_span_cursor(span: &Span, cursor: &SpanCursor) -> std::cmp::Ordering {
    span.start_time_ns
        .cmp(&cursor.start_time_ns)
        .then_with(|| span.end_time_ns.cmp(&cursor.end_time_ns))
        .then_with(|| span.trace_id.cmp(&cursor.trace_id))
        .then_with(|| span.span_id.cmp(&cursor.span_id))
}

fn span_after_cursor(span: &Span, cursor: &SpanCursor) -> bool {
    compare_span_cursor(span, cursor).is_gt()
}

fn first_offset_after(
    segment: &segment::Segment,
    offsets: &[u64],
    cursor: &SpanCursor,
) -> Result<usize> {
    let mut low = 0;
    let mut high = offsets.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let record = segment
            .record_at_offset(offsets[middle])
            .map_err(segment_error)?;
        let span = record_to_span(&record)?;
        if span_after_cursor(&span, cursor) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Ok(low)
}

fn sort_spans(spans: &mut [Span]) {
    spans.sort_by(compare_spans);
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
            .is_some_and(|ext| ext.to_string_lossy() == SEGMENT_SUFFIX.trim_start_matches('.'));
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
        let seg = Box::new(segment::Segment::open(&path).map_err(segment_error)?);
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
                && (name.ends_with(LEGACY_SEGMENT_SUFFIX) || name.ends_with(SEGMENT_SUFFIX))
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
        .strip_suffix(LEGACY_SEGMENT_SUFFIX)
        .or_else(|| stem.strip_suffix(SEGMENT_SUFFIX))?;
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
