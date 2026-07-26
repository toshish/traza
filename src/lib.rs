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

/// How many matches a sorted query will rank before refusing.
///
/// Sorting has to see every match, so an unbounded sorted query over a
/// low-selectivity filter would materialize the store. The limit is generous
/// enough for real triage and small enough to stay bounded.
pub const SORT_CANDIDATE_LIMIT: usize = 200_000;

/// Result ordering for [`SpanFilter::sort`].
///
/// Sorting costs what it always costs: a sorted answer cannot be streamed,
/// because the last record read may belong first. An unsorted query stops as
/// soon as it has `limit` matches; a sorted one must find every match, order
/// them, and then truncate. That is why unsorted stays the default even though
/// "slowest first" is usually the question being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanSort {
    /// Longest duration first — the triage order.
    DurationDesc,
    /// Shortest duration first.
    DurationAsc,
    /// Most recent start first.
    StartDesc,
    /// Oldest start first. Traza's natural order, but stated explicitly.
    StartAsc,
}

impl SpanSort {
    /// Parses the `sort=` query value, or `None` if unrecognized.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "duration_desc" | "-duration" => Some(Self::DurationDesc),
            "duration_asc" | "duration" => Some(Self::DurationAsc),
            "start_desc" | "-start" => Some(Self::StartDesc),
            "start_asc" | "start" => Some(Self::StartAsc),
            _ => None,
        }
    }

    fn compare(self, left: &Span, right: &Span) -> std::cmp::Ordering {
        let duration = |span: &Span| span.end_time_ns.saturating_sub(span.start_time_ns);
        match self {
            Self::DurationDesc => duration(right).cmp(&duration(left)),
            Self::DurationAsc => duration(left).cmp(&duration(right)),
            Self::StartDesc => right.start_time_ns.cmp(&left.start_time_ns),
            Self::StartAsc => left.start_time_ns.cmp(&right.start_time_ns),
        }
    }
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
    /// Match spans whose duration is at most this many nanoseconds.
    pub max_duration_ns: Option<u64>,
    /// Attribute lower bounds, compared numerically. Analytics could already
    /// aggregate token counts and cost while search could not find them, so
    /// "which calls cost more than a cent" was unanswerable.
    pub min_attributes: Vec<(String, f64)>,
    /// Attribute upper bounds, compared numerically.
    pub max_attributes: Vec<(String, f64)>,
    /// Attributes that must NOT equal this value. A span missing the key
    /// entirely matches: "not an error" should include spans that never
    /// recorded a status.
    pub excluded_attributes: Vec<(String, Value)>,
    /// Result ordering. `None` keeps Traza's stable span order, which is the
    /// only order that can stream without materializing.
    pub sort: Option<SpanSort>,
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

/// A named group of write-path defaults trading ingest throughput against
/// per-write latency.
///
/// The knobs on that axis are only coherent when set together — a large
/// `flush_spans` and a zero `wal_commit_window` pull in opposite directions —
/// and setting them together requires knowing what each does to the other. A
/// profile is that knowledge, named, so an operator picks an intent instead of
/// reverse-engineering the internals.
///
/// **A profile cannot change [`Durability`], and that is structural rather
/// than conventional**: no variant carries one, so there is no value of
/// `Profile` that weakens what an acknowledgement means. Durability is a
/// correctness contract with clients, not a performance dial, and a profile
/// named for speed that quietly made writes lossy would be exactly the trap
/// worth designing out. [`Durability::Buffered`] stays an explicit opt-in.
///
/// Compaction is likewise untouched. It trades ingest throughput against
/// *search* latency, not write latency, and disabling it is a cliff (segment
/// count and file descriptors grow without bound) rather than a tuning
/// choice — so every profile leaves it at [`CompactionConfig::default`] and
/// read-path tuning stays on its own flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// Maximize sustained ingest. Waits [`Profile::wal_commit_window`] before
    /// each fsync so more batches share it, and seals segments less often.
    /// Costs per-write latency, most visibly at low concurrency where there is
    /// nothing to batch with and the wait buys nothing.
    Throughput,
    /// The defaults: no deliberate fsync delay, segments sealed every 10,000
    /// spans. What [`Config::default`] gives, so an unset profile changes
    /// nothing.
    #[default]
    Balanced,
    /// Minimize per-write acknowledgement latency and its tail. No fsync
    /// delay, and small segment seals so the stall one write occasionally pays
    /// for the whole buffer stays short.
    Latency,
}

impl Profile {
    /// Parses the CLI name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "throughput" => Some(Self::Throughput),
            "balanced" => Some(Self::Balanced),
            "latency" => Some(Self::Latency),
            _ => None,
        }
    }

    /// The CLI name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Throughput => "throughput",
            Self::Balanced => "balanced",
            Self::Latency => "latency",
        }
    }

    /// Buffered spans this profile seals a segment at.
    ///
    /// A seal is a stall charged to whichever write crosses the threshold, so
    /// this is a tail-latency dial — but NOT a monotonic one, and these values
    /// are measured turning points rather than round numbers (see
    /// `docs/configuration.md`).
    ///
    /// A seal costs a fixed amount (two fsyncs, create/rename, reopen-and-
    /// parse) on top of its per-span cost, so shrinking the threshold pays
    /// that fixed cost more often. Past the turn it buys nothing and costs
    /// everything: measured open loop, the p99 minimum is at 5,000, and
    /// 3,000 is already worse. At 2,000 and below the store cannot sustain
    /// 60k spans/s at all. `Latency` therefore sits at the bottom of the
    /// curve, not at the smallest value available.
    pub fn flush_spans(self) -> usize {
        match self {
            Self::Throughput => 30_000,
            Self::Balanced => 10_000,
            Self::Latency => 5_000,
        }
    }

    /// How long this profile delays an fsync to collect more batches into it.
    ///
    /// Every acknowledgement in the window is delayed by up to this long, so
    /// it is only ever paid for by the amortization it buys. 500us measured
    /// best at small and medium batches and still positive at batch=1000;
    /// beyond about 1 ms the delay costs more than the amortization returns.
    pub fn wal_commit_window(self) -> Option<std::time::Duration> {
        match self {
            Self::Throughput => Some(std::time::Duration::from_micros(500)),
            Self::Balanced | Self::Latency => None,
        }
    }

    /// This profile's defaults as a [`Config`]. Durability and compaction are
    /// whatever [`Config::default`] says; a profile does not set them.
    pub fn config(self) -> Config {
        Config {
            flush_spans: self.flush_spans(),
            wal_commit_window: self.wal_commit_window(),
            ..Config::default()
        }
    }
}

/// Storage configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Automatic flush threshold, counted BOTH ways: unique buffered records,
    /// and upserts admitted since the last flush.
    ///
    /// Counting only unique records made the threshold unreachable for a
    /// workload that keeps updating the same keys — a retried or
    /// progressively-enriched span never adds a record, so the buffer stays
    /// tiny while the log grows with every write. Ingest cost is per
    /// operation, so the threshold is too.
    pub flush_spans: usize,
    /// Write-ahead log bytes that trigger a seal and reclaim the log, or
    /// `None` to leave the log bounded only by [`Self::flush_spans`].
    ///
    /// The record thresholds count logical work; this one bounds the physical
    /// consequence. It is the backstop that keeps the log — and therefore
    /// restart replay time — bounded no matter how large individual spans are
    /// or how the workload distributes over keys. Ignored in
    /// [`Durability::Buffered`], which keeps no log.
    ///
    /// **This is the log's real size bound, and it became load-bearing when
    /// sealing moved off the writer lock.** A seal that empties the buffer
    /// still discards the whole log, so an idle or lightly loaded store
    /// behaves exactly as before. Under sustained ingest the buffer is never
    /// empty at publish time — spans keep arriving while the segment is being
    /// written — and reclaiming to the survivors on every seal would put a
    /// re-serialization of thousands of spans back under the writer lock, most
    /// of what moving the write off it just bought. So the log is reclaimed
    /// when it reaches this bound instead, and the cost amortizes over every
    /// seal since the last reclaim. Leaving records in the log is always safe:
    /// replaying a span a segment already holds upserts it to the same value.
    pub flush_wal_bytes: Option<u64>,
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
    /// How long a thread about to fsync the log waits for more batches to
    /// join it. Zero (the default) syncs immediately.
    ///
    /// Group commit already amortizes fsync across whatever batches happen to
    /// be in flight. This buys MORE amortization by deliberately waiting, and
    /// the cost is exactly what it sounds like: every acknowledgement in the
    /// window is delayed by up to this long. It helps when batches arrive
    /// steadily but concurrency is too low to fill a sync on its own, and it
    /// hurts an idle store, which is why it is off unless asked for. It never
    /// weakens the guarantee — the acknowledgement still follows the fsync.
    pub wal_commit_window: Option<std::time::Duration>,
}

/// Default ceiling on log bytes before a flush seals the buffer. Large enough
/// that ordinary ingest never reaches it before the record threshold does,
/// small enough that a restart replays it in well under a second.
pub const DEFAULT_FLUSH_WAL_BYTES: u64 = 64 * 1024 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            flush_spans: 10_000,
            flush_wal_bytes: Some(DEFAULT_FLUSH_WAL_BYTES),
            ttl_seconds: None,
            payload_threshold: None,
            durability: Durability::Wal,
            compaction: Some(CompactionConfig::default()),
            wal_commit_window: None,
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
    /// A query asked for more work than the store will do — currently, a
    /// sorted query matching more spans than it will rank. Refused rather
    /// than answered approximately: a truncated ranking is a wrong answer
    /// that looks like a right one.
    QueryTooBroad(String),
    /// The write-ahead log is damaged somewhere other than its final append,
    /// so recovering the prefix would silently drop acknowledged batches that
    /// come after the damage. Refusing to open is the only honest answer.
    WalCorrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "storage I/O error: {error}"),
            Self::Json(error) => write!(f, "storage JSON error: {error}"),
            Self::AlreadyOpen => write!(f, "store is already open by another writer"),
            Self::LockPoisoned(name) => write!(f, "storage lock poisoned: {name}"),
            Self::InvalidSpan(reason) => write!(f, "invalid span: {reason}"),
            Self::QueryTooBroad(reason) => write!(f, "query too broad: {reason}"),
            Self::WalCorrupt(detail) => write!(f, "write-ahead log is corrupt: {detail}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::AlreadyOpen
            | Self::LockPoisoned(_)
            | Self::InvalidSpan(_)
            | Self::QueryTooBroad(_)
            | Self::WalCorrupt(_) => None,
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
///
/// **Why the spans are behind an `Arc`.** A seal has to take the buffer's
/// contents away with it and write them with no lock held, while the buffer
/// stays readable — so the drain is a copy, and a deep copy of ten thousand
/// spans is exactly the cost the seal is trying to stop paying under the lock.
/// Sharing the span makes the drain a pointer copy. It also answers the
/// question the seal has to ask when it finishes: *is the value under this key
/// still the one I sealed, or did someone re-ingest it while I was writing?*
/// [`std::sync::Arc::ptr_eq`] answers that exactly. Comparing VALUES would
/// not: a span legitimately re-ingested unchanged is a newer version that
/// happens to look identical, and this codebase has already lost data once to
/// content-based identity (see `recover_supersede_markers`).
#[derive(Debug, Default)]
struct WriteBuffer {
    spans: Vec<std::sync::Arc<Span>>,
    index: std::collections::HashMap<(String, String), usize>,
    /// Spans upserted since the last seal, counting replacements. `spans.len()`
    /// deliberately does not: an update to a buffered key leaves the record
    /// count untouched while still costing a log record, so the flush policy
    /// needs both numbers (see [`Config::flush_spans`]).
    upserts: usize,
}

impl WriteBuffer {
    fn upsert(&mut self, span: Span) {
        let key = (span.trace_id.clone(), span.span_id.clone());
        self.upserts += 1;
        // A fresh allocation every time, including for a replacement: the old
        // handle may be held by a seal in flight, and the seal decides what to
        // evict by comparing handles. Mutating in place would make the newer
        // version indistinguishable from the sealed one and get it evicted.
        let span = std::sync::Arc::new(span);
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

    /// Adopts `spans` as the whole buffer, rebuilding the position index.
    ///
    /// The index maps a primary key to a POSITION, so adopting a vector
    /// without rebuilding it would leave `upsert` overwriting the wrong span.
    fn restore(&mut self, spans: Vec<std::sync::Arc<Span>>) {
        self.spans = spans;
        self.reindex();
    }

    fn retain(&mut self, keep: impl Fn(&Span) -> bool) {
        self.spans.retain(|span| keep(span));
        self.reindex();
    }

    /// Drops every span whose handle `sealed` recognizes, and reindexes.
    ///
    /// The predicate is handle identity rather than key membership: a key
    /// re-ingested while the seal was writing holds a DIFFERENT handle, so it
    /// survives here and is sealed by the next pass. Dropping it by key would
    /// destroy the newer version and leave the segment's older one live.
    fn evict_sealed(&mut self, sealed: &std::collections::HashSet<*const Span>) {
        self.spans
            .retain(|span| !sealed.contains(&std::sync::Arc::as_ptr(span)));
        self.reindex();
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (position, span) in self.spans.iter().enumerate() {
            self.index
                .insert((span.trace_id.clone(), span.span_id.clone()), position);
        }
    }
}

/// Whether a seal may decline to run because another is already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SealWait {
    /// Wait for the permit. Used where the caller was promised a segment:
    /// [`Store::flush`] and [`Durability::Flushed`].
    ForPermit,
    /// Give up if another seal holds the permit. Its drain covers everything
    /// this one would have written, because sealed spans stay in the buffer
    /// until they are published.
    SkipIfBusy,
}

/// What one seal took out of the write buffer, and the id it will publish
/// under. See [`Store::seal`].
#[derive(Debug)]
struct Drained {
    spans: Vec<std::sync::Arc<Span>>,
    /// `WriteBuffer::upserts` as it stood at the drain, so the publish can
    /// subtract exactly what this segment accounted for.
    upserts: usize,
    id: u64,
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
    // Locking discipline: maintenance first, then sealing, then writer, then
    // segments, and retain that order until every guard is dropped. The rollup
    // cache is leaf-level: acquired briefly and never while taking another
    // lock.
    //
    // `maintenance` serializes the two operations that REPLACE segment files —
    // compaction and expiry — against each other. It is not a read lock and
    // not an ingest lock: both proceed throughout, which is the point. It
    // exists so each of those operations can pin its inputs, do its I/O with
    // no engine lock held, and publish under a short revalidated critical
    // section without also having to reason about the other one.
    maintenance: Mutex<()>,
    // `sealing` admits ONE seal at a time, and it is not the writer lock:
    // ingest runs throughout a seal, which is the entire point of
    // [`Store::seal`]. A second seal starting concurrently would buy nothing —
    // sealed spans stay in the buffer until they are published, so the running
    // seal already covers everything the second one would drain — while
    // costing a duplicate copy of the same spans on disk.
    //
    // Expiry takes it too. A seal that drained before a deletion ran would
    // otherwise publish its segment afterwards and resurrect exactly the spans
    // expiry just removed from the buffer, the log, and every segment it knew
    // about. Ingest keeps flowing while expiry holds it — its seals coalesce
    // into the next one — with one exception worth knowing about:
    // [`Durability::Flushed`] must seal before it acknowledges, so under that
    // mode an ingest waits out a running deletion.
    //
    // An ingesting thread NEVER takes this while holding the writer lock: it
    // releases the writer lock first and then seals. The order above is what
    // that rule is written down as.
    sealing: Mutex<()>,
    writer: Mutex<WriteBuffer>,
    // Segments are `Arc` so a reader can PIN the set it resolved against and
    // keep reading after compaction or expiry has replaced the files. A
    // segment holds its own open descriptor, so an unlinked-but-pinned segment
    // still serves its records.
    segments: Mutex<Vec<std::sync::Arc<Segment>>>,
    rollups: Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<analytics::SegmentRollup>>>,
    recent_payloads: payload::TouchRegistry,
    annotations: annotations::AnnotationLog,
    next_segment: AtomicU64,
    /// Seals that have claimed a segment id but not yet published it.
    ///
    /// Read and written ONLY while holding the `segments` lock, which is what
    /// makes it atomic with respect to id claims and is why `Relaxed` is
    /// enough. Compaction consults it before claiming an id of its own: see
    /// [`Store::merge_tail_run`].
    unpublished_seals: AtomicU64,
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
                // Recovery may refuse to proceed (see `Wal::recover`): a log
                // damaged anywhere but its final append cannot be resumed
                // without dropping acknowledged batches, and dropping them
                // quietly is worse than not starting.
                wal::Wal::recover(&directory, |span| buffer.upsert(span))?;
                Some(wal::Wal::open(&directory, config.wal_commit_window)?)
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
                maintenance: Mutex::new(()),
                sealing: Mutex::new(()),
                writer: Mutex::new(buffer),
                segments: Mutex::new(segments),
                rollups: Mutex::new(std::collections::HashMap::new()),
                recent_payloads: payload::TouchRegistry::default(),
                next_segment: AtomicU64::new(next_segment),
                unpublished_seals: AtomicU64::new(0),
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
    ///    under the writer lock, so a concurrent seal cannot drain a buffer
    ///    that disagrees with the log;
    /// 2. release the lock;
    /// 3. fsync, and only then return;
    /// 4. seal, if the buffer has reached one of its bounds.
    ///
    /// The fsync deliberately happens OUTSIDE the lock: that is what lets
    /// concurrent batches accumulate into one sync instead of serializing an
    /// fsync per request. A crash before step 3 loses the batch, which is
    /// correct — nothing was acknowledged yet.
    ///
    /// **Step 4 is outside the lock too, and it is outside step 1-3's lock on
    /// purpose.** Taking the seal permit while holding the writer lock would
    /// invert the lock order (see the `sealing` field) and deadlock against
    /// expiry.
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
        let seal_now;
        let seal_must_wait;
        {
            let waited = Instant::now();
            let mut writer = self.lock_writer()?;
            self.metrics.writer_lock_wait.record(elapsed_nanos(&waited));
            if let (Some(log), Some(frame)) = (&self.wal, &frame) {
                pending_commit = Some(
                    self.metrics
                        .wal_write
                        .time(|| log.append(frame, &self.metrics))?,
                );
            }
            self.metrics.buffer_upsert.time(|| {
                for span in spans {
                    writer.upsert(span);
                }
            });
            seal_now = self.should_flush(&writer);
            seal_must_wait = self.seal_must_not_be_skipped(&writer);
        }

        // `flushed` promises the caller a SEALED segment, so its seal must
        // finish before this returns and it must not be skipped because
        // another one happens to be running — it waits for the permit. Every
        // other mode promises an fsync, which the commit below already
        // provides, so its seal is an optimization that may coalesce into a
        // seal already in flight.
        let synchronous = self.config.durability == Durability::Flushed;
        if seal_now && synchronous {
            // The segment supersedes the log for everything it holds, so the
            // acknowledgement no longer needs the fsync this batch queued.
            pending_commit = None;
            self.seal(SealWait::ForPermit)?;
        }
        if let (Some(log), Some(lsn)) = (&self.wal, pending_commit) {
            log.commit(lsn, &self.metrics)?;
        }
        if seal_now && !synchronous {
            let wait = match seal_must_wait {
                true => SealWait::ForPermit,
                false => SealWait::SkipIfBusy,
            };
            self.seal(wait)?;
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
    ///
    /// Synchronous: when this returns, the spans that were buffered when it
    /// was called are in a segment on disk. Spans ingested by other threads
    /// WHILE it runs may still be buffered afterwards — they arrived after the
    /// snapshot this call sealed, and the next seal takes them.
    pub fn flush(&self) -> Result<()> {
        self.seal(SealWait::ForPermit)
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
                latest.insert(span.span_id.clone(), Span::clone(span));
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
        attribute_union_view(&writer, &segments, keys, values)
    }

    /// Returns spans matching `filter` strictly after `cursor`.
    ///
    /// The cursor is compared using the same total order as [`Self::query`],
    /// allowing callers to paginate with a constant result bound even when
    /// many spans share one timestamp.
    ///
    /// Each call resolves against the store as it is NOW. Paginating a
    /// changing store therefore walks a moving dataset; [`Self::snapshot`] is
    /// the fixed one.
    pub fn query_after(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
    ) -> Result<Vec<Span>> {
        // A session predicate unions candidates across recognized keys, which
        // the single-key attribute index cannot express — resolve it up front,
        // then apply the remaining predicates, order, and limit.
        if let Some(session_id) = &filter.session {
            let spans = self.resolve_session_spans(session_id)?;
            return Ok(narrow_session_spans(spans, filter, cursor));
        }
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        query_view(&writer, &segments, &self.metrics, filter, cursor)
    }

    /// Pins the store as it is now and returns a view that keeps answering
    /// from that instant.
    ///
    /// A multi-page read that re-queries the live store between pages is not
    /// reading one dataset: a span ingested mid-read can be missed, and one
    /// re-ingested under a key an earlier page already emitted comes back a
    /// second time — an export could report "complete" over output holding two
    /// versions of the same primary key. A pinned view has neither problem,
    /// because nothing about it changes after it is taken.
    ///
    /// What pinning costs: the write buffer is copied (at most
    /// [`Config::flush_spans`] spans), and the segment files live at least as
    /// long as the view. Compaction and expiry are free to unlink them
    /// meanwhile — the view reads through descriptors it already holds — but
    /// the bytes are not reclaimed until it drops, so a view is meant to be
    /// held for one operation, not parked.
    pub fn snapshot(&self) -> Result<SnapshotView<'_>> {
        // Lock order: writer before segments (see Store field docs).
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut buffer = WriteBuffer::default();
        buffer.restore(writer.spans.clone());
        Ok(SnapshotView {
            buffer,
            segments: segments.clone(),
            metrics: &self.metrics,
        })
    }
}

/// An immutable, pinned view of the store: the write buffer as it stood when
/// the view was taken, plus the segments live at that moment. See
/// [`Store::snapshot`].
///
/// Reads answer exactly as the store would have at that instant, under the
/// same primary-key precedence, and keep doing so however the store changes
/// afterwards.
#[derive(Debug)]
pub struct SnapshotView<'a> {
    buffer: WriteBuffer,
    segments: Vec<std::sync::Arc<Segment>>,
    /// Reads through a view are real reads and are counted as such. Borrowing
    /// the store's counters is also what keeps a view from outliving the store
    /// whose files it pins.
    metrics: &'a metrics::Metrics,
}

impl SnapshotView<'_> {
    /// Spans matching `filter`, in Traza's stable span order.
    pub fn query(&self, filter: &SpanFilter) -> Result<Vec<Span>> {
        self.query_after(filter, None)
    }

    /// Spans matching `filter` strictly after `cursor`, in Traza's stable span
    /// order. Paging through a view with this is what makes a multi-page read
    /// one coherent dataset.
    pub fn query_after(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
    ) -> Result<Vec<Span>> {
        if let Some(session_id) = &filter.session {
            let spans =
                analytics::resolve_session_spans_in(&self.buffer, &self.segments, session_id)?;
            return Ok(narrow_session_spans(spans, filter, cursor));
        }
        query_view(&self.buffer, &self.segments, self.metrics, filter, cursor)
    }

    /// Number of segments the view pins, for diagnostics and tests.
    pub fn pinned_segment_count(&self) -> usize {
        self.segments.len()
    }
}

/// Applies the remaining predicates, order and limit to session candidates.
/// Session resolution unions several attribute keys, so it cannot be expressed
/// as one indexed filter; everything else about the query still applies.
fn narrow_session_spans(
    mut spans: Vec<Span>,
    filter: &SpanFilter,
    cursor: Option<&SpanCursor>,
) -> Vec<Span> {
    spans.retain(|span| {
        span_matches(span, filter)
            && cursor.map_or(true, |position| span_after_cursor(span, position))
    });
    order_spans(&mut spans, filter.sort);
    if let Some(limit) = filter.limit {
        spans.truncate(limit);
    }
    spans
}

/// Every span carrying `values` under ANY of `keys`, over one buffer and
/// segment set. See [`Store::query_attribute_union`] for why the caller must
/// hold both halves still while this runs.
pub(crate) fn attribute_union_view(
    buffer: &WriteBuffer,
    segments: &[std::sync::Arc<Segment>],
    keys: &[&str],
    values: &[Value],
) -> Result<Vec<Span>> {
    let matches = |span: &Span| {
        keys.iter().any(|key| {
            span.attributes
                .get(*key)
                .is_some_and(|held| values.iter().any(|value| held == value))
        })
    };

    let mut result: Vec<Span> = Vec::new();
    let mut claimed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // The buffer holds the newest version of anything it carries.
    for span in buffer.spans.iter() {
        if matches(span) {
            claimed.insert((span.trace_id.clone(), span.span_id.clone()));
            result.push(Span::clone(span));
        }
    }
    // Any key present in the buffer supersedes every segment copy, even
    // one the predicate does not select in its buffered version.
    for (trace_id, span_id) in buffer.index.keys() {
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

/// Resolves `filter` (and an optional `cursor`) over one buffer and segment
/// set, under Traza's primary-key precedence: the buffer wins, then the newest
/// segment.
///
/// The caller decides what "one set" means — the live store holds both locks
/// across the call, a [`SnapshotView`] owns its copy — but it must be still
/// for the duration either way. See invariant 6 in docs/internals/invariants.md.
pub(crate) fn query_view(
    writer: &WriteBuffer,
    segments: &[std::sync::Arc<Segment>],
    metrics: &metrics::Metrics,
    filter: &SpanFilter,
    cursor: Option<&SpanCursor>,
) -> Result<Vec<Span>> {
    // A sorted answer cannot be streamed: the record that belongs first may be
    // the last one read, so `limit` cannot stop the scan early. Re-run
    // unlimited, order, then truncate. Refusing past a ceiling rather than
    // truncating first is deliberate — a "ten slowest" that silently ranked
    // the first ten thousand matches would be wrong, and wrong quietly.
    if let Some(sort) = filter.sort {
        let unlimited = SpanFilter {
            limit: None,
            sort: None,
            ..filter.clone()
        };
        let mut spans = query_view(writer, segments, metrics, &unlimited, cursor)?;
        if spans.len() > SORT_CANDIDATE_LIMIT {
            return Err(Error::QueryTooBroad(format!(
                "sorting {} matches exceeds the {SORT_CANDIDATE_LIMIT} candidate limit; \
                 narrow the filter (time range, service, or an attribute) and retry",
                spans.len()
            )));
        }
        spans.sort_by(|left, right| {
            sort.compare(left, right)
                .then_with(|| compare_spans(left, right))
        });
        if let Some(limit) = filter.limit {
            spans.truncate(limit);
        }
        return Ok(spans);
    }
    {
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
                .map(|span| Span::clone(span))
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
                // Skip whole segments that cannot hold a matching timestamp.
                // This is the only filter that eliminates a segment without
                // reading it, and "the last N minutes" is the commonest
                // filter an observability store sees.
                metrics.segments_examined.increment();
                if !seg.may_contain_timestamps(filter.since_ns, filter.until_ns) {
                    metrics.segments_pruned_by_time.increment();
                    continue;
                }
                let offsets = select_probe(seg, filter);
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
            metrics.segments_examined.increment();
            if !seg.may_contain_timestamps(filter.since_ns, filter.until_ns) {
                metrics.segments_pruned_by_time.increment();
                continue;
            }
            let offsets = select_probe(seg, filter);
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
                result.push(Span::clone(span));
            }
        }

        sort_spans(&mut result);
        Ok(result)
    }
}

impl Store {
    /// Returns current buffer, segment, physical-record, and disk statistics.
    ///
    /// Persisted counts are physical records rather than logical
    /// last-write-wins cardinality. Naming that distinction explicitly keeps
    /// this operation O(number of segments) instead of decoding the corpus.
    pub fn stats(&self) -> Result<Stats> {
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let persisted_records = segments.iter().map(|segment| segment.record_count()).sum();
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
        let _maintenance = self.lock_maintenance()?;
        let removed = self.expire_before_locked(cutoff_ns)?;
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
    ///
    /// **Reads and ingest continue throughout.** A merge parses every input,
    /// materializes the union, and fsyncs the replacement; holding the segment
    /// lock across that stopped every query, and a blocked query holding the
    /// writer lock then stopped ingest behind it — a multi-gigabyte merge
    /// became a multi-gigabyte outage. Inputs are pinned instead
    /// ([`Store::snapshot`] pins the same way), all of the work happens with
    /// no engine lock held, and only the swap takes the lock back.
    pub fn compact_segments(&self) -> Result<usize> {
        let Some(settings) = self.config.compaction else {
            return Ok(0);
        };
        if settings.fanout < 2 {
            return Ok(0);
        }
        // One rewriter at a time. Compaction and expiry both replace segment
        // files; serializing them is what lets each pin its inputs and trust
        // that only ingest can have changed the set by publication time.
        let _maintenance = self.lock_maintenance()?;
        let mut merged_away = 0usize;
        // Each pass merges one run; a merge can create a run one tier up, so
        // loop until nothing qualifies. Bounded by the tier count.
        loop {
            let segments = self.lock_segments()?;
            let Some(run) = tail_run_to_merge(&segments, &settings) else {
                break;
            };
            drop(segments);
            let merged = self.merge_tail_run(run, &settings)?;
            if merged == 0 {
                // The run went stale between the scan and the publish. Stop
                // rather than spin; the next scheduled pass picks it up.
                break;
            }
            merged_away += merged;
        }
        Ok(merged_away)
    }

    /// Merges the last `run` segments into one. Returns segments removed, or
    /// zero if the run stopped qualifying before the result could be published.
    fn merge_tail_run(&self, run: usize, settings: &CompactionConfig) -> Result<usize> {
        // ---- pin: short critical section -------------------------------
        let (inputs, id) = {
            let segments = self.lock_segments()?;
            // Re-check under the lock: the set may have changed since the scan.
            if tail_run_to_merge(&segments, settings) != Some(run) {
                return Ok(0);
            }
            // A seal that has already claimed a LOWER id but not yet published
            // it would end up sorting before this merge's output, which holds
            // strictly older data — last-write-wins, inverted. The id claims
            // are ordered by this lock, so declining here is enough: once no
            // seal is outstanding, every future one claims above this merge.
            // Bailing costs nothing; the next compaction tick rescans.
            if self.unpublished_seals.load(Ordering::Relaxed) > 0 {
                return Ok(0);
            }
            let start = segments.len() - run;
            let inputs: Vec<std::sync::Arc<Segment>> = segments[start..].to_vec();
            // The id is claimed HERE, under the lock, and together with the
            // check above that is what keeps a concurrent seal ordered
            // correctly. No seal is between its drain and its publish at this
            // instant, so every segment that appears while this merge runs
            // claims a HIGHER id — and therefore sorts after the merged
            // output, which is exactly right: it was written later. Claiming
            // the id after the merge would invert that and let merged (older)
            // versions win over freshly sealed ones.
            let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
            (inputs, id)
        };
        let input_paths: Vec<PathBuf> = inputs.iter().map(|segment| segment.path.clone()).collect();

        // ---- merge: no engine lock held --------------------------------
        // Oldest first, so a later segment's version of a key overwrites an
        // earlier one — the same last-write-wins rule reads apply.
        let mut latest: std::collections::HashMap<(String, String), Span> =
            std::collections::HashMap::new();
        let mut order: Vec<(String, String)> = Vec::new();
        for segment in &inputs {
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

        let new_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}");
        // Journal every input before the replacement exists, so recovery can
        // finish the merge from either side without inspecting content.
        let mut markers = Vec::with_capacity(inputs.len());
        for path in &input_paths {
            let old_name = path
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
        let new_path = new_segment.path.clone();

        // ---- publish: short critical section, revalidated ---------------
        let published = {
            let mut segments = self.lock_segments()?;
            match run_position(&segments, &inputs) {
                Some(start) => {
                    segments.splice(start..start + run, [std::sync::Arc::new(new_segment)]);
                    segments.sort_by(|left, right| left.path.cmp(&right.path));
                    true
                }
                None => false,
            }
        };
        if !published {
            // Nothing was replaced, so nothing may look replaced. Remove the
            // orphan BEFORE its markers: a crash in the other order would let
            // recovery see a complete replacement and delete inputs it does
            // not actually supersede.
            let _ = fs::remove_file(&new_path);
            for marker in markers {
                let _ = fs::remove_file(marker);
            }
            return Ok(0);
        }

        // The merged segment is durable and visible, so the inputs are dead.
        // A reader that pinned them keeps its own descriptors and finishes
        // undisturbed; unlinking only takes the names away. (That is POSIX
        // semantics. On a platform that refuses to unlink an open file this
        // errors while an export is reading, and the merge is simply retried
        // on the next tick — the store stays correct either way.)
        for path in &input_paths {
            unlink_segment(path)?;
        }
        // Durable before the markers are dropped: a marker is what lets
        // recovery finish an unlink that a crash rolled back, so it must
        // outlive any unlink that is not yet durable.
        sync_directory(&self.directory)?;
        for marker in markers {
            let _ = fs::remove_file(marker);
        }
        // Rollups are keyed by path; the inputs' entries are now dead.
        if let Ok(mut rollups) = self.rollups.lock() {
            rollups.retain(|path, _| !input_paths.contains(path));
        }
        Ok(input_paths.len().saturating_sub(1))
    }

    /// Removes spans ending before `cutoff_ns` and returns the number removed.
    ///
    /// **Expiry is a deletion, and a deletion has to reach the log.** Dropping
    /// a span from the write buffer leaves the write-ahead log record that
    /// carried it intact, so the next restart replays it and the expired span
    /// is back — retention a restart undoes is not retention, and for anyone
    /// deleting telemetry on request it is not deletion either. The log is
    /// rewritten to exactly what survived.
    ///
    /// **Reads and ingest continue throughout**, as in
    /// [`Self::compact_segments`]: segments are pinned, rewritten with no
    /// engine lock held, and swapped in one at a time.
    pub fn expire_before(&self, cutoff_ns: u64) -> Result<usize> {
        let _maintenance = self.lock_maintenance()?;
        self.expire_before_locked(cutoff_ns)
    }

    /// [`Self::expire_before`] with the maintenance lock already held.
    fn expire_before_locked(&self, cutoff_ns: u64) -> Result<usize> {
        // A seal in flight drained its spans BEFORE this deletion ran and
        // publishes its segment AFTER, so without this permit an expiry could
        // clean the buffer, the log and every segment it knew about, and then
        // watch a segment land holding exactly the spans it just deleted.
        // Waiting for the permit also keeps a new seal from starting until the
        // deletion is complete. Ingest is unaffected: it never takes this
        // lock, its seals simply coalesce into the next one.
        let _permit = self
            .sealing
            .lock()
            .map_err(|_| Error::LockPoisoned("sealing"))?;

        // ---- buffer and log --------------------------------------------
        // Durable state moves FIRST, memory second, and the step that moves
        // memory cannot fail. Dropping the span from the buffer before the log
        // rewrite succeeded left the two disagreeing on failure — and worse,
        // left nothing for a retry to find: the next call saw an already-clean
        // buffer, reported that it had removed nothing, and never repaired the
        // log, so the restart resurrected the span. An expiry that returns an
        // error must leave the store exactly as retryable as it found it.
        let mut removed = {
            let mut writer = self.lock_writer()?;
            let expired = writer
                .spans
                .iter()
                .filter(|span| span.end_time_ns < cutoff_ns)
                .count();
            if expired > 0 {
                if let Some(log) = &self.wal {
                    // Borrowed, not cloned: the frame is serialized from
                    // references, so computing the survivors costs pointers
                    // rather than a copy of the buffer.
                    let survivors: Vec<&Span> = writer
                        .spans
                        .iter()
                        .filter(|span| span.end_time_ns >= cutoff_ns)
                        .map(|span| span.as_ref())
                        .collect();
                    log.rewrite(&survivors)?;
                }
                writer.retain(|span| span.end_time_ns >= cutoff_ns);
            }
            expired
        };

        // ---- segments: pinned, rewritten with no engine lock held -------
        // Each survivor set is written to a temp file and renamed ONTO the
        // segment it replaces. In place is not an optimization: segment path
        // order is recency order, so handing the survivors a fresh (highest)
        // id would move an old segment to the newest position and let its
        // versions win over segments that legitimately supersede them. The
        // rename is also what makes a supersede marker unnecessary here —
        // there is no window where a replacement exists beside its original.
        let pinned: Vec<std::sync::Arc<Segment>> = self.lock_segments()?.clone();
        for segment in &pinned {
            let all = segment.spans_parsed()?;
            let total = all.len();
            let mut kept: Vec<Span> = all
                .into_iter()
                .filter(|span| span.end_time_ns >= cutoff_ns)
                .collect();
            if kept.len() == total {
                continue;
            }
            removed += total - kept.len();
            sort_spans(&mut kept);

            let replacement = match kept.is_empty() {
                true => None,
                false => Some(std::sync::Arc::new(
                    self.rewrite_segment_in_place(&segment.path, &kept)?,
                )),
            };
            // Off disk FIRST, out of the live list second — same rule as the
            // buffer above. Removing it from the list first meant a failed
            // unlink left the store reporting a segment that is still there,
            // with nothing for the retry to find and the file waiting to be
            // loaded again at the next open. A pinned reader is undisturbed by
            // the unlink: it holds its own descriptor.
            if replacement.is_none() {
                unlink_segment(&segment.path)?;
                // An unlink is visible immediately and durable only when the
                // directory entry it removed is synced. Reporting the deletion
                // before that would let a crash bring the file — and the spans
                // TTL just removed — back.
                sync_directory(&self.directory)?;
            }
            // Publish this one before rewriting the next, so an I/O failure
            // partway through leaves everything already rewritten correctly
            // represented rather than stranded. Nothing below can fail.
            {
                let mut segments = self.lock_segments()?;
                if let Some(position) = segments
                    .iter()
                    .position(|held| std::sync::Arc::ptr_eq(held, segment))
                {
                    match &replacement {
                        Some(replacement) => segments[position] = replacement.clone(),
                        None => {
                            segments.remove(position);
                        }
                    }
                }
            }
            // Rollups are keyed by path, and this path now holds different
            // bytes (or none) — a cached rollup would still count the spans
            // that were just expired.
            if let Ok(mut rollups) = self.rollups.lock() {
                rollups.remove(&segment.path);
            }
        }
        Ok(removed)
    }

    /// Returns copies of the spans in each persisted segment.
    ///
    /// This intentionally narrow inspection hook exists so integration tests
    /// can verify the on-disk invariant that every segment is internally sorted.
    pub fn persisted_segment_spans(&self) -> Result<Vec<Vec<Span>>> {
        let _writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        segments
            .iter()
            .map(|segment| segment.spans_parsed())
            .collect()
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

    /// **Approximate** bytes held resident by the persisted segments' decoded
    /// indexes.
    ///
    /// The necessary companion to [`Self::resident_payload_bytes`], which by
    /// design counts only the payload encoding and is therefore zero however
    /// large the indexes get. Reading that zero as "nothing is resident"
    /// is the mistake this method exists to prevent: `Segment::open` decodes
    /// the whole attribute index, whose keys are complete attribute values,
    /// and keeps it for the life of the segment. See
    /// [`segment::Segment::approx_index_bytes`] for exactly what is and is
    /// not counted — it is a floor, not an allocator measurement.
    pub fn resident_index_bytes(&self) -> Result<usize> {
        let segments = self.lock_segments()?;
        Ok(segments
            .iter()
            .map(|segment| segment.seg.approx_index_bytes())
            .sum())
    }

    /// Distinct indexed `(key, value)` attribute pairs held resident across
    /// all persisted segments, summed per segment.
    ///
    /// Summed, not deduplicated: a value present in two segments is decoded
    /// and retained twice, and that double count is the real resident cost.
    pub fn resident_attribute_index_entries(&self) -> Result<usize> {
        let segments = self.lock_segments()?;
        Ok(segments
            .iter()
            .map(|segment| segment.seg.attribute_index_len())
            .sum())
    }

    fn lock_writer(&self) -> Result<MutexGuard<'_, WriteBuffer>> {
        self.writer
            .lock()
            .map_err(|_| Error::LockPoisoned("writer"))
    }

    fn lock_segments(&self) -> Result<MutexGuard<'_, Vec<std::sync::Arc<Segment>>>> {
        self.segments
            .lock()
            .map_err(|_| Error::LockPoisoned("segments"))
    }

    fn lock_maintenance(&self) -> Result<MutexGuard<'_, ()>> {
        self.maintenance
            .lock()
            .map_err(|_| Error::LockPoisoned("maintenance"))
    }

    /// Whether the buffer has reached any of the bounds a seal enforces.
    ///
    /// Three bounds, because one number cannot express them all. Unique
    /// buffered records bound MEMORY. Upserts since the last seal bound WORK:
    /// a workload that keeps updating the same keys — retries, spans enriched
    /// as they complete — adds records to the log without ever adding one to
    /// the buffer, so a record-only threshold is simply never reached and the
    /// log grows without limit. Log bytes bound the physical consequence and
    /// therefore restart replay, whatever the span sizes or key distribution
    /// turn out to be.
    fn should_flush(&self, writer: &WriteBuffer) -> bool {
        // `flushed` acknowledges only sealed spans, so every call seals.
        if self.config.durability == Durability::Flushed {
            return !writer.is_empty();
        }
        if self.config.flush_spans > 0
            && (writer.len() >= self.config.flush_spans
                || writer.upserts >= self.config.flush_spans)
        {
            return true;
        }
        match (self.config.flush_wal_bytes, &self.wal) {
            (Some(limit), Some(log)) => limit > 0 && log.size_bytes() >= limit,
            _ => false,
        }
    }

    /// Whether ingest should WAIT for the seal permit rather than let its seal
    /// coalesce into the one already running.
    ///
    /// Sealing under the writer lock throttled ingest for free: a thread could
    /// not admit a batch while a seal was running, so the buffer could never
    /// outgrow its threshold by more than one batch. Sealing off the lock
    /// removes that, and removes it in the one direction that matters — if
    /// ingest sustainably outruns sealing, the buffer grows without bound and
    /// the process runs out of memory rather than pushing back.
    ///
    /// So the throttle is put back, deliberately and only at the extreme. Up
    /// to this much overshoot the seal stays fully concurrent, which is the
    /// point of the change; past it an ingesting thread blocks until the
    /// running seal publishes, and the buffer stops growing. Four times the
    /// threshold, because normal overshoot is "whatever arrives during one
    /// seal" and a seal that has fallen four thresholds behind is not
    /// overshoot, it is a store that cannot keep up.
    fn seal_must_not_be_skipped(&self, writer: &WriteBuffer) -> bool {
        self.config.flush_spans > 0 && writer.len() >= self.config.flush_spans.saturating_mul(4)
    }

    /// Seals the write buffer into one segment, doing the I/O with no engine
    /// lock held.
    ///
    /// The shape is [`Self::merge_tail_run`]'s, for the same reasons:
    ///
    /// 1. **Drain under a short lock.** The spans are copied out as shared
    ///    handles and the segment id is claimed. Both under the lock, both
    ///    cheap.
    /// 2. **Write with nothing held.** Converting spans to records, encoding
    ///    the segment, writing it, fsyncing it, renaming it, fsyncing the
    ///    directory and reopening the result was the single largest thing this
    ///    engine did while holding the lock every ingesting thread needs. It
    ///    touches only a private vector; it never needed the lock at all.
    /// 3. **Publish under a short lock, then reconcile.**
    ///
    /// **The drain is a copy, not a removal, and that is the invariant.**
    /// Taking the spans out at step 1 would leave already-acknowledged data in
    /// neither the buffer nor a segment for the whole of step 2 — briefly
    /// invisible to readers, which is exactly what [`Self::get_trace`]
    /// promises cannot happen. The merge never removes data from visibility
    /// either: its inputs stay live and pinned until the output is published.
    /// A seal does the same, and evicts only afterwards.
    ///
    /// **The segment id is claimed at drain time, not at write time.** Segment
    /// path order IS recency order here, so two seals completing out of order
    /// with ids claimed at completion would silently invert last-write-wins.
    /// The permit means two seals cannot currently overlap, so this is not the
    /// only thing standing between the store and that bug — but it is the one
    /// that states the invariant locally, rather than leaving segment ordering
    /// as a consequence of how seals happen to be scheduled.
    /// [`Self::merge_tail_run`] claims its id early for the same reason.
    fn seal(&self, wait: SealWait) -> Result<()> {
        let _permit = match wait {
            SealWait::ForPermit => self
                .sealing
                .lock()
                .map_err(|_| Error::LockPoisoned("sealing"))?,
            SealWait::SkipIfBusy => match self.sealing.try_lock() {
                Ok(permit) => permit,
                Err(std::sync::TryLockError::WouldBlock) => {
                    // Everything this seal would have drained is still in the
                    // buffer, so the running seal already covers it.
                    self.metrics.segment_seals_coalesced.increment();
                    return Ok(());
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Error::LockPoisoned("sealing"))
                }
            },
        };
        self.seal_with_permit()
    }

    /// [`Self::seal`] with the seal permit already held.
    fn seal_with_permit(&self) -> Result<()> {
        let sealing = Instant::now();

        // ---- drain: short critical section ------------------------------
        // Every `segment_seal_locked` sample starts AFTER the guard is
        // acquired. Timing from before it would fold in lock WAIT, which is
        // already `writer_lock_wait`, and would make the in-lock total exceed
        // wall clock under contention — a number that cannot be summed is not
        // a saturation measurement.
        let drained = {
            let writer = self.lock_writer()?;
            let locked = Instant::now();
            if writer.is_empty() {
                return Ok(());
            }
            // Cloning a `Vec<Arc<Span>>` copies pointers, not spans. This is
            // why the buffer holds handles: the same drain over `Vec<Span>`
            // would deep-copy ten thousand spans under the lock and give back
            // a good part of what moving the write off it just bought.
            let spans = writer.spans.clone();
            let upserts = writer.upserts;
            // Claimed under the segments lock, which is what orders it against
            // a concurrent compaction: see the doc comment above and
            // `merge_tail_run`. The in-flight count is published under the
            // same lock so compaction can see that a lower id is outstanding.
            let _segments = self.lock_segments()?;
            self.unpublished_seals.fetch_add(1, Ordering::Relaxed);
            let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .segment_seal_locked
                .record(elapsed_nanos(&locked));
            Drained { spans, upserts, id }
        };

        // ---- write: no engine lock held ---------------------------------
        let mut pending = drained.spans;
        pending.sort_by(|left, right| compare_spans(left, right));
        let written = self.write_segment(drained.id, &pending);
        let segment = match written {
            Ok(segment) => segment,
            Err(error) => {
                // Nothing was published and nothing was removed — the buffer
                // and the log still hold every acknowledged span exactly as
                // they did before. A failed seal is a no-op, retried by the
                // next one.
                let _unclaim = self.lock_segments()?;
                self.unpublished_seals.fetch_sub(1, Ordering::Relaxed);
                return Err(error);
            }
        };

        // ---- publish and reconcile: short critical sections --------------
        {
            let mut segments = self.lock_segments()?;
            let locked = Instant::now();
            segments.push(std::sync::Arc::new(segment));
            segments.sort_by(|left, right| left.path.cmp(&right.path));
            self.unpublished_seals.fetch_sub(1, Ordering::Relaxed);
            self.metrics
                .segment_seal_locked
                .record(elapsed_nanos(&locked));
        }
        let sealed = pending.len() as u64;
        // `pending` stays alive across the reconcile below: the eviction test
        // is handle identity, and identity by address is only meaningful while
        // every address in the set still belongs to a live allocation.
        self.reconcile_after_publish(&pending, drained.upserts)?;
        drop(pending);
        self.metrics.segment_seal.record(elapsed_nanos(&sealing));
        self.metrics.segment_seal_spans.add(sealed);
        Ok(())
    }

    /// Drops the published spans from the buffer and reclaims the log.
    ///
    /// **Ordering.** The segment is already fsynced, renamed and visible, so
    /// it — not the log — is now the durable authority for everything in
    /// `published`. That is what makes both removals below legal, and it is
    /// why they happen after the publish rather than before it.
    ///
    /// Within this function the rule from the log-reclamation review still
    /// holds: **the durable change happens before the in-memory change it
    /// stands for.** The log is rewritten to the survivors first and the
    /// buffer drops the sealed spans second, so a rewrite that fails leaves
    /// the buffer untouched and the whole reconcile retryable by the next
    /// seal. The reverse order left memory ahead of the recovery authority
    /// with nothing for a retry to find.
    fn reconcile_after_publish(
        &self,
        published: &[std::sync::Arc<Span>],
        upserts_at_drain: usize,
    ) -> Result<()> {
        let sealed: std::collections::HashSet<*const Span> =
            published.iter().map(std::sync::Arc::as_ptr).collect();

        let mut writer = self.lock_writer()?;
        let locked = Instant::now();
        if let Some(log) = &self.wal {
            let survivors: Vec<&Span> = writer
                .spans
                .iter()
                .filter(|span| !sealed.contains(&std::sync::Arc::as_ptr(span)))
                .map(|span| span.as_ref())
                .collect();
            if survivors.is_empty() {
                // Nothing acknowledged is outside a segment any more, so the
                // whole log goes. Truncation, not a staged rewrite.
                log.reset()?;
            } else if self.log_needs_reclaiming(log) {
                // Reclaiming on EVERY seal would put a re-encode of the
                // survivors back under the writer lock — at a sustained
                // 250k spans/s that is thousands of spans re-serialized per
                // seal, which is most of what moving the write off the lock
                // just bought. Leaving records in the log is always safe:
                // replaying a span the segment already holds upserts it to the
                // same value. So the log is reclaimed on the bound that
                // exists to bound it, `flush_wal_bytes`, and the cost
                // amortizes across every seal since the last reclaim.
                log.rewrite(&survivors)?;
            }
        }
        writer.evict_sealed(&sealed);
        // The upserts that produced this segment are accounted for; the ones
        // admitted while it was being written are not, and must still count
        // toward the next seal. Zeroing the counter instead would let an
        // update-heavy workload postpone its seals indefinitely, which is the
        // failure `upserts` exists to prevent.
        writer.upserts = writer.upserts.saturating_sub(upserts_at_drain);
        self.metrics
            .segment_seal_locked
            .record(elapsed_nanos(&locked));
        Ok(())
    }

    /// Whether the log has grown past the bound that exists to bound it.
    fn log_needs_reclaiming(&self, log: &wal::Wal) -> bool {
        match self.config.flush_wal_bytes {
            Some(limit) => limit > 0 && log.size_bytes() >= limit,
            None => false,
        }
    }

    fn write_segment<S: std::borrow::Borrow<Span>>(&self, id: u64, spans: &[S]) -> Result<Segment> {
        let file_name = format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}");
        let final_path = self.directory.join(&file_name);
        if final_path.exists() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "segment id collision: {} already exists",
                    final_path.display()
                ),
            )));
        }
        self.seal_segment(&final_path, spans)
    }

    /// Rewrites an existing segment to hold exactly `spans`.
    ///
    /// Same name, same id, same place in the segment order — see
    /// [`Self::expire_before`] for why that is a correctness requirement and
    /// not a convenience. The rename replaces the file atomically, so a crash
    /// leaves either the original or the survivors and never both.
    fn rewrite_segment_in_place(&self, path: &Path, spans: &[Span]) -> Result<Segment> {
        self.seal_segment(path, spans)
    }

    /// Encodes `spans`, fsyncs them into a temp file, and renames that onto
    /// `final_path`. The rename is what makes a segment appear atomically: a
    /// reader sees the whole file or none of it, never a partial one.
    fn seal_segment<S: std::borrow::Borrow<Span>>(
        &self,
        final_path: &Path,
        spans: &[S],
    ) -> Result<Segment> {
        let file_name = final_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".{file_name}.{}.{}.tmp", std::process::id(), counter);
        let temp_path = self.directory.join(temp_name);

        let write_result = (|| {
            let records = spans
                .iter()
                .map(|span| span_to_record(span.borrow()))
                .collect::<Result<Vec<_>>>()?;
            let encoded = segment::encode(&records).map_err(segment_error)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(&temp_path)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&temp_path, final_path)?;
            sync_directory(&self.directory)?;
            let bytes = fs::metadata(final_path)?.len();
            // Reopen FILE-BACKED: the encoded buffer is dropped and the
            // segment serves reads from disk immediately — flushing never
            // leaves a resident payload copy behind.
            drop(encoded);
            let seg = Box::new(segment::Segment::open(final_path).map_err(segment_error)?);
            Ok(Segment {
                path: final_path.to_path_buf(),
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
fn tail_run_to_merge(
    segments: &[std::sync::Arc<Segment>],
    settings: &CompactionConfig,
) -> Option<usize> {
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

/// Where `run` sits in `segments`, by identity, or `None` if it is no longer
/// there as one contiguous block.
///
/// Identity rather than path: this is the revalidation a merge does before
/// publishing, and it has to answer "are these the same segments I pinned",
/// which a path cannot say once files start being replaced in place.
fn run_position(
    segments: &[std::sync::Arc<Segment>],
    run: &[std::sync::Arc<Segment>],
) -> Option<usize> {
    if run.is_empty() || segments.len() < run.len() {
        return None;
    }
    (0..=segments.len() - run.len()).find(|start| {
        segments[*start..*start + run.len()]
            .iter()
            .zip(run)
            .all(|(held, pinned)| std::sync::Arc::ptr_eq(held, pinned))
    })
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

/// Orders by an explicit request, falling back to Traza's stable span order.
/// The stable order is also the tie-break, so equal keys never come back in a
/// different sequence between two identical queries.
fn order_spans(spans: &mut [Span], sort: Option<SpanSort>) {
    match sort {
        None => sort_spans(spans),
        Some(sort) => spans.sort_by(|left, right| {
            sort.compare(left, right)
                .then_with(|| compare_spans(left, right))
        }),
    }
}

/// Picks the index probe that narrows the read the most.
///
/// Only ONE predicate can drive the scan — the rest are checked per record —
/// so which one is chosen decides how much work the query does. This used to
/// be a fixed order: service, then name, then whichever attribute happened to
/// come first. That order tends to pick the WORST candidate, because
/// `service` is usually the least selective thing in a trace store: with
/// twenty services and a hundred distinct attribute values, probing by
/// service reads five times more records than probing by attribute, and then
/// discards them. Adding a precise filter to a service query made it slower.
///
/// Posting lists are already materialized in the index, so their lengths are
/// exact, not estimated — the smallest one is genuinely the least work.
fn select_probe<'a>(seg: &'a segment::Segment, filter: &SpanFilter) -> &'a [u64] {
    let mut best: Option<&'a [u64]> = None;
    let mut consider = |offsets: &'a [u64]| {
        // `Option::is_none_or` would read better but is newer than this
        // crate's MSRV.
        if best.map_or(true, |current| offsets.len() < current.len()) {
            best = Some(offsets);
        }
    };
    if let Some(service) = &filter.service {
        consider(seg.attribute_posting_offsets_ref(IDX_SERVICE, service));
    }
    if let Some(name) = &filter.name {
        consider(seg.attribute_posting_offsets_ref(IDX_NAME, name));
    }
    for (key, value) in &filter.attributes {
        // Session keys are expanded by the caller into a union and cannot
        // drive a single probe.
        if key.starts_with('\u{0}') {
            continue;
        }
        consider(seg.attribute_posting_offsets_ref(key, &canonical_value(value)));
    }
    // Nothing indexable: every record is a candidate.
    best.unwrap_or_else(|| seg.record_offsets())
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
    if filter
        .max_duration_ns
        .is_some_and(|maximum| span.end_time_ns.saturating_sub(span.start_time_ns) > maximum)
    {
        return false;
    }
    if !filter
        .min_attributes
        .iter()
        .all(|(key, bound)| numeric_attribute(span, key).is_some_and(|value| value >= *bound))
    {
        return false;
    }
    if !filter
        .max_attributes
        .iter()
        .all(|(key, bound)| numeric_attribute(span, key).is_some_and(|value| value <= *bound))
    {
        return false;
    }
    // A missing key is NOT an exclusion: `not_attr.status=error` means "not
    // known to be an error", which includes spans carrying no status at all.
    if filter
        .excluded_attributes
        .iter()
        .any(|(key, value)| span.attributes.get(key) == Some(value))
    {
        return false;
    }
    filter
        .attributes
        .iter()
        .all(|(key, value)| attribute_equals(span, key, value))
}

/// An attribute read as a number, whether it was stored as one or as a string.
///
/// Instrumentation is inconsistent about this — OpenLLMetry emits token counts
/// as integers, some SDKs stringify them — and a range filter that only
/// understood one encoding would silently miss half a corpus.
fn numeric_attribute(span: &Span, key: &str) -> Option<f64> {
    match span.attributes.get(key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Equality that does not depend on how the value was typed on the wire.
///
/// `attr.code=200` used to parse as JSON whenever the text was valid JSON, so
/// it matched the NUMBER 200 and could never match the STRING "200" — and an
/// empty result set is indistinguishable from no such data. Both readings now
/// match, which is the behaviour a caller who typed `200` expects.
fn attribute_equals(span: &Span, key: &str, value: &Value) -> bool {
    match span.attributes.get(key) {
        None => false,
        Some(stored) if stored == value => true,
        Some(stored) => scalar_text(stored)
            .zip(scalar_text(value))
            .is_some_and(|(stored, wanted)| stored == wanted),
    }
}

/// The text form of a scalar, for cross-type comparison. Containers return
/// `None`: two arrays that render alike are not the same array.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn load_segments(directory: &Path) -> Result<Vec<std::sync::Arc<Segment>>> {
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
        segments.push(std::sync::Arc::new(Segment {
            path,
            bytes: bytes_meta,
            seg,
        }));
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

/// Unlinks a segment file, treating "already gone" as success.
///
/// Deletion is retried on failure, and a retry has to be able to make progress
/// after a partial one: an unlink can land while the directory sync that makes
/// it durable does not, so the next attempt legitimately finds the file
/// missing. Failing on `NotFound` would turn that into a permanent error over
/// state that is already correct.
fn unlink_segment(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
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
pub(crate) fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_directory: &Path) -> Result<()> {
    Ok(())
}

/// The two rules an unlocked seal rests on, tested where they can be stated
/// deterministically instead of raced for.
///
/// Both are invisible from query results under any single-threaded test, which
/// is exactly why they get their own tests here rather than being left to the
/// concurrency suite to stumble over.
#[cfg(test)]
mod seal_tests {
    use super::*;

    fn span(trace: &str, id: &str, name: &str) -> Span {
        serde_json::from_value(serde_json::json!({
            "trace_id": trace, "span_id": id, "name": name, "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 1_100u64,
            "attributes": {}
        }))
        .expect("span")
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("traza-seal-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn sealed_set(spans: &[std::sync::Arc<Span>]) -> std::collections::HashSet<*const Span> {
        spans.iter().map(std::sync::Arc::as_ptr).collect()
    }

    #[test]
    fn eviction_drops_exactly_what_was_sealed() {
        let mut buffer = WriteBuffer::default();
        buffer.upsert(span("t", "a", "v1"));
        buffer.upsert(span("t", "b", "v1"));
        let drained = buffer.spans.clone();

        buffer.evict_sealed(&sealed_set(&drained));
        assert!(buffer.is_empty(), "both sealed keys are gone");
        assert!(!buffer.contains_key("t", "a"));
    }

    #[test]
    fn a_key_reingested_during_a_seal_survives_the_publish() {
        // The whole reason the buffer holds handles rather than values. The
        // seal drains "v1", "v2" is ingested while the segment is being
        // written, and the publish must NOT treat the key as sealed: the
        // segment holds v1, so dropping v2 would leave the older version live
        // and lose an acknowledged write.
        let mut buffer = WriteBuffer::default();
        buffer.upsert(span("t", "a", "v1"));
        buffer.upsert(span("t", "b", "v1"));
        let drained = buffer.spans.clone();

        buffer.upsert(span("t", "a", "v2"));
        buffer.evict_sealed(&sealed_set(&drained));

        assert_eq!(buffer.len(), 1, "only the re-ingested key remains");
        assert_eq!(buffer.spans[0].span_id, "a");
        assert_eq!(
            buffer.spans[0].name, "v2",
            "the newer version survived the seal that carried the older one"
        );
        // The index has to follow the eviction, or the next upsert of this key
        // writes over the wrong slot.
        assert!(buffer.contains_key("t", "a"));
        assert!(!buffer.contains_key("t", "b"));
    }

    #[test]
    fn an_identical_reingest_is_still_a_newer_version() {
        // Identity, not equality. A span re-ingested unchanged is a NEWER
        // version that happens to look the same; a value comparison would call
        // it sealed and drop it, which is the same content-based-identity
        // mistake `recover_supersede_markers` exists to avoid.
        let mut buffer = WriteBuffer::default();
        buffer.upsert(span("t", "a", "v1"));
        let drained = buffer.spans.clone();
        buffer.upsert(span("t", "a", "v1"));

        buffer.evict_sealed(&sealed_set(&drained));
        assert_eq!(
            buffer.len(),
            1,
            "an unchanged re-ingest is a live buffer entry, not a sealed one"
        );
    }

    #[test]
    fn compaction_declines_while_a_seal_holds_an_unpublished_id() {
        // A seal that claimed a LOWER id but has not published it yet would
        // sort BEFORE a merge that claims its id now — and the merge's output
        // is strictly older data, so it would win. Compaction must wait.
        let dir = test_dir("compaction-guard");
        let config = Config {
            durability: Durability::Buffered,
            compaction: Some(CompactionConfig {
                fanout: 2,
                base_bytes: 1,
                max_segment_bytes: 0,
            }),
            ..Config::default()
        };
        let store = Store::open(&dir, config).expect("open");
        for index in 0..4 {
            store
                .ingest(span("t", &format!("s{index}"), "v1"))
                .expect("ingest");
            store.flush().expect("flush");
        }
        assert_eq!(store.stats().expect("stats").segment_count, 4);

        // Stand in for a seal that has claimed an id and not yet published.
        store.unpublished_seals.store(1, Ordering::Relaxed);
        assert_eq!(
            store.compact_segments().expect("compact"),
            0,
            "compaction must not claim an id above an outstanding seal's"
        );
        assert_eq!(store.stats().expect("stats").segment_count, 4);

        store.unpublished_seals.store(0, Ordering::Relaxed);
        assert!(
            store.compact_segments().expect("compact") > 0,
            "and must proceed as soon as none is outstanding"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod planner_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Builds a one-segment store shape in memory: 100 records sharing one
    /// service, 1 of them carrying a rare attribute.
    fn segment_with_skewed_selectivity() -> segment::Segment {
        let records: Vec<segment::RecordInput> = (0..100)
            .map(|index| {
                let mut attributes = BTreeMap::new();
                // IDX_SERVICE is the synthetic key the engine indexes
                // `service` under; every record shares one value, so its
                // posting list is the whole segment.
                attributes.insert(IDX_SERVICE.to_owned(), "svc".to_owned());
                attributes.insert(
                    "rare".to_owned(),
                    canonical_value(&Value::String(if index == 7 {
                        "yes".to_owned()
                    } else {
                        "no".to_owned()
                    })),
                );
                segment::RecordInput {
                    timestamp: 1_000 + index,
                    trace_id: format!("t{index}"),
                    attributes,
                    payload: b"{}".to_vec(),
                }
            })
            .collect();
        let bytes = segment::encode(&records).expect("encode");
        segment::Segment::from_bytes(bytes).expect("open")
    }

    #[test]
    fn the_probe_is_the_smallest_posting_list_not_the_first_predicate() {
        // The regression this guards: the planner took a fixed order
        // (service, then name, then first attribute), and `service` is
        // usually the LEAST selective thing in a trace store. Adding a
        // precise attribute to a service query then read 100 records instead
        // of 1 and discarded 99 — adding a filter made the query slower.
        //
        // Asserted as a unit test because this is a performance property:
        // both plans return identical results, so no correctness test can
        // tell them apart (a mutation check confirmed exactly that).
        let seg = segment_with_skewed_selectivity();

        let service_only = SpanFilter {
            service: Some("svc".to_owned()),
            ..SpanFilter::default()
        };
        assert_eq!(
            select_probe(&seg, &service_only).len(),
            100,
            "service alone can only probe the whole segment"
        );

        let both = SpanFilter {
            service: Some("svc".to_owned()),
            attributes: vec![("rare".to_owned(), Value::String("yes".to_owned()))],
            ..SpanFilter::default()
        };
        assert_eq!(
            select_probe(&seg, &both).len(),
            1,
            "with a selective attribute available, the scan must follow it"
        );

        let no_predicate = SpanFilter::default();
        assert_eq!(
            select_probe(&seg, &no_predicate).len(),
            100,
            "with nothing indexable every record is a candidate"
        );
    }

    #[test]
    fn a_predicate_matching_nothing_probes_nothing() {
        // The best possible plan for an impossible predicate is an empty
        // candidate list, not a full scan that filters everything out.
        let seg = segment_with_skewed_selectivity();
        let absent = SpanFilter {
            service: Some("svc".to_owned()),
            attributes: vec![("rare".to_owned(), Value::String("absent".to_owned()))],
            ..SpanFilter::default()
        };
        assert!(select_probe(&seg, &absent).is_empty());
    }
}
