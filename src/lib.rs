#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A small durable storage engine for tracing spans.
//!
//! Spans are buffered in memory and periodically persisted as sorted,
//! file-backed indexed segments. Reads combine the buffered and persisted data.

pub mod analytics;
pub mod annotations;
pub mod auth;
pub mod content;
pub mod erasure;
pub mod evals;
pub mod expiration;
mod generation;
pub mod hash;
pub mod insights;
pub mod mcp;
mod media;
pub mod metrics;
pub mod otlp;
pub mod otlp_pb;
pub mod payload;
pub mod pricing;
mod rollup_file;
pub mod seed;
pub mod segment;
pub mod semconv;
pub mod tail;
pub mod ui;
mod wal;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
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
/// Reserved index key carrying a record's tenant. Inserted only when the
/// tenant is non-empty, so a single-tenant store's segments stay byte-for-byte
/// what they were before tenancy existed — and tenant-scoped queries and
/// tenant-subject erasures get an index-served prefilter when it matters.
const IDX_TENANT: &str = "\u{0}tenant";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An event attached to a span.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Event {
    /// Event name.
    pub name: String,
    /// Event timestamp in nanoseconds since the Unix epoch. Deserialization
    /// accepts the documented wire aliases, `time_unix_nano` among them: an
    /// event carries the OTLP spelling far more often than the canonical one,
    /// because clients build events from the same shape they send over OTLP.
    /// A span whose timestamps were accepted under an alias and whose events
    /// were not is the worst of both — the batch 400s on a field the caller
    /// spelled the way the rest of the ecosystem spells it.
    #[serde(
        alias = "time_unix_nano",
        alias = "timestamp_unix_nano",
        alias = "time_ns",
        alias = "time"
    )]
    pub timestamp_ns: u64,
    /// Arbitrary event attributes. Defaulted, like the span's own: an event is
    /// frequently just a named instant, and requiring `{}` to say so rejected
    /// the whole batch over a field with an obvious empty value.
    #[serde(default)]
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
    /// (with the trace id, the within-tenant half of the primary key):
    /// distinct spans sharing an empty span_id would collide into one
    /// upserted key.
    pub span_id: String,
    /// The tenant this span belongs to. Empty is the DEFAULT tenant, and is
    /// never serialized, so a single-tenant deployment writes byte-identical
    /// records to what it wrote before tenancy existed. Non-empty values are
    /// constrained to `[a-z0-9][a-z0-9._-]{0,63}` at ingest — lowercase only,
    /// because an identity that needs canonicalization is an identity that
    /// will one day be compared uncanonicalized (the payload-hash lesson).
    ///
    /// **The wire and storage key is `$tenant`, not `tenant`.** A span's
    /// top-level namespace is open — [`Self::extra`] preserves any unknown
    /// field — so a bare `tenant` key is client data and must stay client
    /// data. Before tenancy that is exactly where it lived, and a store
    /// written then reads back now without a value silently becoming an
    /// identity no query selects and no erasure names. The `$` sigil marks a
    /// reserved identity the way `$payload` marks a reserved reference, so the
    /// discriminator is the key itself, not a guess about a value's shape.
    ///
    /// The narrow honesty: what is new is the *reservation* of `$tenant`, not
    /// the `$` sigil, which predates tenancy through `$payload`. A store
    /// written by tenant-unaware code has an empty identity by construction —
    /// nothing wrote the field — and the only bytes that could carry a literal
    /// top-level `$tenant` are a *foreign* pre-tenancy store that happened to
    /// use that exact key for its own data. No store of ours writes such
    /// bytes, and the pre-1.0 terms do not promise reading foreign ones; a
    /// pre-tenancy import path would have to fold such a key back into
    /// [`Self::extra`] itself. A bound credential still stamps this field
    /// server-side regardless of the body.
    #[serde(rename = "$tenant", default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
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
    /// Match only spans whose completion status equals this.
    ///
    /// This reads the span's own `status` field, NOT an attribute. The two are
    /// easy to confuse — `attr.status=error` matches an *attribute* named
    /// `status`, which most instrumentation never writes — and without this
    /// field "show me the failures" was unanswerable even though every
    /// aggregate in the store already counted errors from `Span::status`.
    pub status: Option<String>,
    /// Statuses that must NOT match. Unlike [`Self::excluded_attributes`], a
    /// span always has a status, so there is no missing-key case: an empty
    /// status is a value like any other and `not_status=error` keeps it.
    pub excluded_statuses: Vec<String>,
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
    /// Match spans whose text contains every word of this query.
    ///
    /// This is **word search, not substring search**: `refund` matches a span
    /// saying "Refund the order" and does not match one saying "refunds". A
    /// multi-word query is a conjunction, not a phrase — the words may appear
    /// anywhere, in any order, across any of the span's text. The text
    /// searched is every string value in the span's attributes and its events'
    /// attributes, plus event names.
    ///
    /// **A value offloaded to the payload store is searchable only within the
    /// preview kept inline** (256 characters). Offloading happens at ingest,
    /// before anything indexes the span, so the rest of the text is not
    /// present to be indexed or matched. With the server's default
    /// `--payload-threshold-bytes` of 256 KiB almost nothing is offloaded and
    /// this does not arise; it matters if you lower the threshold.
    ///
    /// The semantics are set by what the segment's content index can safely
    /// over-approximate; see [`crate::content`] for why substring matching
    /// would produce wrong answers rather than slow ones.
    pub content: Option<String>,
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
    /// Match only spans of this tenant. `None` matches every tenant (the
    /// operator view); `Some("")` matches the default tenant explicitly;
    /// `Some(t)` matches tenant `t`. This is THE tenant-scoping choke point:
    /// a credential bound to a tenant has this forced on every query it can
    /// express, and every span-filter surface — search, export, tail, series,
    /// duration, failures, slowest — inherits the predicate by building this
    /// struct rather than by remembering to check.
    pub tenant: Option<String>,
    /// Maximum number of returned spans.
    pub limit: Option<usize>,
}

/// Exclusive position in Traza's stable span order.
///
/// Passing a cursor to [`Store::query_after`] returns only spans ordered after
/// `(start_time_ns, end_time_ns, tenant, trace_id, span_id)`. This is the
/// bounded pagination primitive used by dataset export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanCursor {
    /// Span start timestamp.
    pub start_time_ns: u64,
    /// Span end timestamp.
    pub end_time_ns: u64,
    /// Tenant; empty for the default tenant.
    pub tenant: String,
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
            tenant: span.tenant.clone(),
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
        }
    }
}

/// The base64url alphabet, unpadded — safe in a query string without escaping.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

impl SpanCursor {
    /// Encodes the cursor as one opaque URL-safe token.
    ///
    /// Opaque on purpose: a client that cannot parse the token cannot come to
    /// depend on its layout, which leaves the ordering key free to change. The
    /// payload is `version | start | end | tenant_len | tenant | trace_len |
    /// trace | span`, so an id containing any byte — including the delimiters
    /// a text encoding would need — round-trips exactly. The leading version
    /// byte exists because the layout HAS changed once (tenancy): without it,
    /// a pre-tenancy token could parse into a valid wrong cursor — a silently
    /// wrong page, the exact failure this parser promises to prevent.
    pub fn to_token(&self) -> String {
        let tenant = self.tenant.as_bytes();
        let trace = self.trace_id.as_bytes();
        let span = self.span_id.as_bytes();
        let mut raw = Vec::with_capacity(25 + tenant.len() + trace.len() + span.len());
        raw.push(CURSOR_VERSION);
        raw.extend_from_slice(&self.start_time_ns.to_be_bytes());
        raw.extend_from_slice(&self.end_time_ns.to_be_bytes());
        raw.extend_from_slice(&(tenant.len() as u32).to_be_bytes());
        raw.extend_from_slice(tenant);
        raw.extend_from_slice(&(trace.len() as u32).to_be_bytes());
        raw.extend_from_slice(trace);
        raw.extend_from_slice(span);
        // `div_ceil` is stable only since 1.73; this crate supports 1.70.
        let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
        for chunk in raw.chunks(3) {
            let bits = (u32::from(chunk[0]) << 16)
                | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
                | u32::from(chunk.get(2).copied().unwrap_or(0));
            let take = chunk.len() + 1;
            for index in 0..take {
                out.push(B64[((bits >> (18 - 6 * index)) & 0x3F) as usize] as char);
            }
        }
        out
    }

    /// Parses a token produced by [`Self::to_token`].
    ///
    /// Returns `None` for anything that is not one, so a stale or hand-edited
    /// cursor is a `400` rather than a silently wrong page.
    pub fn from_token(token: &str) -> Option<Self> {
        let mut raw = Vec::with_capacity(token.len() * 3 / 4);
        let mut bits = 0u32;
        let mut have = 0u32;
        for byte in token.bytes() {
            let value = B64.iter().position(|candidate| *candidate == byte)? as u32;
            bits = (bits << 6) | value;
            have += 6;
            if have >= 8 {
                have -= 8;
                raw.push((bits >> have) as u8);
            }
        }
        if raw.len() < 25 || raw[0] != CURSOR_VERSION {
            return None;
        }
        let start_time_ns = u64::from_be_bytes(raw[1..9].try_into().ok()?);
        let end_time_ns = u64::from_be_bytes(raw[9..17].try_into().ok()?);
        let tenant_len = u32::from_be_bytes(raw[17..21].try_into().ok()?) as usize;
        let rest = raw.get(21..)?;
        let tenant = rest.get(..tenant_len)?;
        let rest = rest.get(tenant_len..)?;
        let trace_len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let rest = rest.get(4..)?;
        let trace = rest.get(..trace_len)?;
        let span = rest.get(trace_len..)?;
        Some(Self {
            start_time_ns,
            end_time_ns,
            tenant: String::from_utf8(tenant.to_vec()).ok()?,
            trace_id: String::from_utf8(trace.to_vec()).ok()?,
            span_id: String::from_utf8(span.to_vec()).ok()?,
        })
    }
}

/// Version byte leading every cursor token. Bumped when the ordering key
/// changes shape; a token from another version parses to `None`, never to a
/// plausible wrong position.
const CURSOR_VERSION: u8 = 1;

/// What one query actually had to do, reported back with its results.
///
/// Traza's argument is that a filtered search is cheap; a dashboard that never
/// says how cheap asks its users to take that on faith. These are the numbers
/// behind the claim, counted per query rather than sampled from the process-wide
/// counters — those race under concurrent readers and cannot be attributed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryCost {
    /// Segments the query considered, pruned or not.
    pub segments_examined: u32,
    /// Segments skipped whole because their timestamp range could not hold a
    /// match. The work a time filter avoided.
    pub segments_pruned: u32,
    /// Wall-clock nanoseconds spent inside the engine, excluding serialization.
    pub elapsed_ns: u64,
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
///
/// `PartialEq` but not `Eq`: pricing carries per-million-token rates, and a
/// float has no total equality to offer.
#[derive(Clone, Debug, PartialEq)]
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
    /// Longest a span may sit in the write buffer before a seal is due on age
    /// alone, or `None` to bound the buffer only by volume.
    ///
    /// The volume thresholds assume traffic: a trickle that never reaches
    /// [`Self::flush_spans`] leaves acknowledged spans in the buffer — and in
    /// the log a restart must replay — indefinitely. Worse than replay time,
    /// a buffered UPSERT of a persisted span disqualifies every segment
    /// holding that key from answering aggregations out of its rollup, and
    /// the disqualification lasts exactly as long as the key stays buffered
    /// (measured on a real deployment: 36 days, because the store wrote ~150
    /// spans a day against a 10,000-span threshold). Age is the bound volume
    /// cannot provide.
    ///
    /// Enforced on the ingest path opportunistically and by
    /// [`Store::maintain_buffer`] on whatever cadence the caller schedules —
    /// like TTL, the engine implements the policy and the embedder owns the
    /// clock. `traza-server` calls it from its maintenance tick.
    pub max_buffer_age: Option<std::time::Duration>,
    /// Whether an aggregation that observes segment-versus-segment key
    /// shadowing may schedule a bounded deduplicating merge. On by default;
    /// inert without [`Self::compaction`], which owns all merging.
    ///
    /// "Shadowing" is the state where the same `(trace_id, span_id)` exists
    /// in more than one segment, so last-write-wins has versions to arbitrate
    /// at read time and no shadowed segment may answer from its rollup. The
    /// analytics fold detects it for free while deciding exactly that; this
    /// switch lets the observation latch a flag that
    /// [`Store::maintain_buffer`] converts into a merge of the shadowed tail
    /// run — bounded by the compaction size cap, cooled down after success,
    /// backed off exponentially when nothing mergeable is found. The trigger
    /// is the observed mechanism, never a latency threshold: it fires exactly
    /// when a query had to decode instead of using a rollup, and stops firing
    /// the moment the duplicates are merged away.
    ///
    /// Deliberately NOT latched by buffer-caused shadowing — a buffered
    /// upsert of a persisted key. A merge cannot retire that state (the
    /// buffer is not mergeable, and a client still updating the key re-forms
    /// it immediately); [`Self::max_buffer_age`] is what bounds it.
    pub shadow_seal: bool,
    /// Retention period in seconds; zero disables TTL expiration.
    pub ttl_seconds: Option<u64>,
    /// Per-tenant retention overrides, in seconds, keyed by tenant name. A
    /// tenant listed here expires on its own window; a tenant not listed
    /// falls back to [`Config::ttl_seconds`] — and if that is unset, NEVER
    /// expires, whatever other tenants are configured with. Retention is a
    /// per-tenant policy the moment tenants exist; a single window forced on
    /// every tenant would make one customer's compliance clock another's
    /// data loss.
    pub tenant_ttl_seconds: std::collections::HashMap<String, u64>,
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
    /// Whether sealed segments carry a content index, making
    /// [`SpanFilter::content`] fast. On by default.
    ///
    /// Turning it off does not remove the ability to search — a segment with
    /// no content index is scanned instead of skipped, so the same query
    /// returns the same rows and simply costs what a scan costs. What it
    /// removes is the index's price: tokenizing every span's text at seal
    /// time, and roughly 1-2% of segment size on disk.
    ///
    /// Leave it on unless a measurement says otherwise. It is exposed mainly
    /// so that the measurement is possible: with it, the same corpus can be
    /// queried with and without the index and the difference attributed.
    pub content_index: bool,
    /// Spans retained in the live tail's admission ring (see [`tail`]).
    ///
    /// Whichever of this and [`Self::tail_ring_bytes`] binds first decides how
    /// far behind a subscriber may fall before it is told it gapped. Neither
    /// bound is a correctness one — falling behind is reported, never silently
    /// skipped — but a count alone is not a memory bound, which is why there
    /// are two.
    pub tail_ring_spans: usize,
    /// Bytes of span text the live tail's admission ring may hold.
    ///
    /// **This is what actually bounds the tail's memory.** The ring becomes the
    /// sole owner of a span once a seal evicts it from the write buffer, and
    /// span size varies by orders of magnitude: an LLM span carrying a prompt
    /// just under [`Self::payload_threshold`] is roughly a thousand times a
    /// span carrying a status code. Counted only by entries, a default ring of
    /// such spans reached hundreds of megabytes — the same text residency the
    /// attribute index was rewritten to remove.
    pub tail_ring_bytes: usize,
    /// Per-model rates used to derive a cost for LLM spans that did not meter
    /// one. Empty by default, which derives nothing and is exactly the
    /// behaviour every store had before this existed.
    ///
    /// A metered `llm.cost_usd` always wins; this only fills blanks. The
    /// table's [fingerprint](crate::pricing::Pricing::fingerprint) is part of
    /// a rollup sidecar's binding, so changing rates invalidates the cached
    /// counters computed under the old ones rather than silently reporting
    /// them forever. See [`crate::pricing`].
    pub pricing: std::sync::Arc<crate::pricing::Pricing>,
}

/// The last commit whose reader accepts every superseded segment format.
///
/// A commit rather than a release tag, and one rather than several, because
/// neither alternative works for a real store. A store accumulates segments in
/// whichever format was current when each was sealed, so it can hold several at
/// once — naming "the release that wrote v2" sends an operator to a build that
/// cannot read the v3 segments sitting beside it. And formats 4 and 5 were
/// never tagged, so for those there is no release to name at all.
///
/// This commit reads 2 through 5 (`MIN_READABLE_VERSION` 2, `VERSION` 5), which
/// covers every indexed format this project has written. Format 1 was JSONL and
/// is refused separately, with its own message.
pub const LEGACY_SEGMENT_READER: &str = "cf40bea";

/// Default ceiling on log bytes before a flush seals the buffer. Large enough
/// that ordinary ingest never reaches it before the record threshold does,
/// small enough that a restart replays it in well under a second.
pub const DEFAULT_FLUSH_WAL_BYTES: u64 = 64 * 1024 * 1024;

/// Default ceiling on how long a buffered span may wait for a seal.
///
/// Five minutes: long enough that a store under any real traffic reaches a
/// volume threshold first and never seals on age at all, short enough that a
/// trickle deployment's restart replay, durability exposure, and
/// rollup-disqualifying buffered upserts are all bounded by a coffee break
/// rather than by the arrival rate. Seals are off the critical path and
/// coalesce, so the cost of an age seal is one small segment for tier-0
/// compaction to fold — a few milliseconds — which is why this defaults on
/// rather than opt-in.
pub const DEFAULT_MAX_BUFFER_AGE: std::time::Duration = std::time::Duration::from_secs(300);

/// Base spacing between shadow passes, and the value futility backoff resets
/// to after a successful merge.
const SHADOW_PASS_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Spacing after a shadow pass that merged. A workload that re-poisons the
/// store after every merge — a client continually re-upserting keys that keep
/// landing in fresh segments — is thereby bounded to four tail rewrites an
/// hour instead of one per observation window.
const SHADOW_MERGE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Ceiling on futility backoff: a store whose collision cannot be merged
/// within the byte budget settles at one scan per hour, not one per minute.
const SHADOW_BACKOFF_CEILING: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The shadow pass's schedule state; see [`Store::finish_shadow_pass`].
#[derive(Debug)]
struct ShadowPassClock {
    /// Earliest the next pass may run; `None` means immediately, so the first
    /// observation after open is answered without waiting out an interval.
    not_before: Option<Instant>,
    /// The current futility backoff, doubled on each pass that merges nothing
    /// and reset by one that does.
    backoff: std::time::Duration,
}

impl Default for ShadowPassClock {
    fn default() -> Self {
        Self {
            not_before: None,
            backoff: SHADOW_PASS_MIN_INTERVAL,
        }
    }
}

/// Default admission-ring depth, in spans.
///
/// Enough to cover minutes of a quiet store or seconds of a loud one —
/// comfortably longer than a browser tab spends backgrounded between
/// reconnects. On narrow spans this is the binding bound; on wide ones
/// [`DEFAULT_TAIL_RING_BYTES`] binds first, which is the point of having both.
pub const DEFAULT_TAIL_RING_SPANS: usize = 8_192;

/// Default ceiling on the bytes the admission ring holds.
///
/// Chosen as a bound an operator does not have to think about: small enough
/// that a tail cannot meaningfully change a process's footprint, large enough
/// that ordinary spans reach the count bound first and the byte bound only
/// engages for the wide-text workloads where it matters.
pub const DEFAULT_TAIL_RING_BYTES: usize = 32 * 1024 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            flush_spans: 10_000,
            flush_wal_bytes: Some(DEFAULT_FLUSH_WAL_BYTES),
            max_buffer_age: Some(DEFAULT_MAX_BUFFER_AGE),
            shadow_seal: true,
            ttl_seconds: None,
            tenant_ttl_seconds: std::collections::HashMap::new(),
            payload_threshold: None,
            durability: Durability::Wal,
            compaction: Some(CompactionConfig::default()),
            wal_commit_window: None,
            content_index: true,
            tail_ring_spans: DEFAULT_TAIL_RING_SPANS,
            tail_ring_bytes: DEFAULT_TAIL_RING_BYTES,
            pricing: std::sync::Arc::new(crate::pricing::Pricing::default()),
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
    /// Seconds the oldest buffered span has waited for a seal, or `None` when
    /// the buffer is empty. The number [`Config::max_buffer_age`] bounds;
    /// watching it climb past that bound means nothing is scheduling
    /// [`Store::maintain_buffer`].
    pub buffer_age_seconds: Option<u64>,
}

/// What one ingest call actually did.
///
/// `accepted` spans are stored under the configured [`Durability`].
/// `suppressed` spans were acknowledged and deliberately NOT stored, because
/// a pending erasure covered them — the admission barrier that makes an
/// erasure's cut exact. The split exists so an ingest surface never reports
/// a suppressed span as durable: a `200` whose body says `accepted: 1` about
/// a span that never reached memory or the log would be a durability claim
/// with nothing behind it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Admission {
    /// Spans stored under the configured durability mode.
    pub accepted: usize,
    /// Spans dropped by the erasure admission barrier.
    pub suppressed: usize,
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
    /// A filter carried a predicate the surface it was given to cannot honour.
    ///
    /// Refused rather than ignored. Dropping a predicate answers a different
    /// question than the caller asked while looking like it answered theirs,
    /// which is how a live tail came to silently discard every span that
    /// started before its window.
    UnsupportedFilter(&'static str),
    /// A request named something that does not hold — an unknown dataset, a
    /// parent version from another dataset, an example outside its
    /// experiment's manifest. The client's error, with the offender named.
    Invalid(String),
    /// A request conflicts with the store's current state rather than being
    /// malformed: a tenant with an erasure pending, a tombstoned version, a
    /// payload reference whose bytes are gone. Retryable once the state
    /// changes, which is exactly what HTTP 409 means.
    Conflict(String),
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
            Self::UnsupportedFilter(reason) => write!(f, "unsupported filter: {reason}"),
            Self::WalCorrupt(detail) => write!(f, "write-ahead log is corrupt: {detail}"),
            Self::Invalid(reason) => write!(f, "invalid request: {reason}"),
            Self::Conflict(reason) => write!(f, "conflict: {reason}"),
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
            | Self::WalCorrupt(_)
            | Self::UnsupportedFilter(_)
            | Self::Invalid(_)
            | Self::Conflict(_) => None,
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
    /// FNV-1a hashes of every primary key in this segment, computed on first
    /// use and kept for the segment's lifetime — the supersede prefilter (see
    /// [`superseded_by_newer`]).
    ///
    /// Per-instance rather than in the store-level rollup cache because an
    /// instance IS one immutable generation of the file: a pinned reader can
    /// outlive a TTL rewrite of its path, and a set bound to the instance
    /// cannot be served for the wrong generation. Index-scale on purpose —
    /// eight bytes per distinct key, the same size story as the sidecar it is
    /// loaded from — where the full rollup this duplicates a corner of is
    /// counter-heavy and stays in the evictable cache.
    key_hashes: std::sync::OnceLock<std::collections::HashSet<u64>>,
}

fn canonical_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Every canonical encoding under which a span holding a value EQUAL to
/// `value` — by [`attribute_equals`] — could have been indexed.
///
/// The index is keyed on canonical JSON, so `200` and `"200"` occupy two
/// different entries. But `attribute_equals` deliberately treats them as the
/// same value, because instrumentation is inconsistent about whether a status
/// code or a token count is a number or a string. Probing only the caller's
/// own encoding therefore made the index a FILTER rather than a superset: the
/// other encoding's records were never candidates, so the type-agnostic
/// comparison never got to see them and a query returned half its matches.
///
/// This is the rule every index in this crate obeys and the reason all of them
/// are checked again against the decoded record: **an index may only narrow
/// the work, never the answer.**
fn attribute_encodings(value: &Value) -> Vec<String> {
    let mut encodings = vec![canonical_value(value)];
    // Containers are compared structurally, so there is no cross-type form.
    let Some(text) = scalar_text(value) else {
        return encodings;
    };
    let mut push = |candidate: &Value| {
        let canonical = canonical_value(candidate);
        if !encodings.contains(&canonical) {
            encodings.push(canonical);
        }
    };
    push(&Value::String(text.clone()));
    // A number stored as a number. Parsing through serde_json is what keeps
    // this consistent with how the value would have been canonicalized on the
    // way in — "0200" and "1.50" are not JSON numbers that round-trip, and
    // their scalar text would not have matched anyway.
    if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
        if parsed.is_number() && scalar_text(&parsed).as_deref() == Some(text.as_str()) {
            push(&parsed);
        }
    }
    match text.as_str() {
        "true" => push(&Value::Bool(true)),
        "false" => push(&Value::Bool(false)),
        _ => {}
    }
    encodings
}

/// Candidate offsets for an attribute equality, unioned across every encoding
/// the value could have been stored under, in record order.
///
/// Record order is load-bearing: the lazy k-way merge and `first_offset_after`
/// both require ascending offsets, so the union is sorted and deduplicated
/// rather than concatenated.
fn attribute_candidates<'a>(seg: &'a segment::Segment, key: &str, value: &Value) -> Cow<'a, [u64]> {
    let encodings = attribute_encodings(value);
    if encodings.len() == 1 {
        return Cow::Borrowed(seg.attribute_candidate_offsets(key, &encodings[0]));
    }
    let mut merged: Vec<u64> = encodings
        .iter()
        .flat_map(|encoding| {
            seg.attribute_candidate_offsets(key, encoding)
                .iter()
                .copied()
        })
        .collect();
    merged.sort_unstable();
    merged.dedup();
    Cow::Owned(merged)
}

/// Every string a content search looks in, borrowed from the span.
///
/// One definition serves both sides: the encoder tokenizes exactly these
/// strings into the segment's content index, and the query verifies against
/// exactly these strings. Two traversals that disagreed — one indexing event
/// attributes, say, and the other not — would produce a filter that finds
/// spans the index cannot, or worse, misses spans it should have found.
fn content_strs(span: &Span) -> Vec<&str> {
    fn collect<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(text) => out.push(text),
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            // Objects recurse normally, with ONE field skipped: the value of
            // a `$payload` key. That key marks an offloaded value, whose
            // reference carries a 64-character hex digest nobody will search
            // for — indexing it only costs filter bits.
            //
            // Skipping the field rather than special-casing the whole object
            // is deliberate. `$payload` is Traza's marker, but nothing stops a
            // tool call's arguments from having a field of that name, and
            // treating any object that has one as a reference removed every
            // sibling field from search. Recursing everywhere else gives the
            // same result for a genuine reference — its other fields are
            // `bytes` (a number, not text) and `preview` — while ordinary
            // nested data stays searchable, and it needs no guess about
            // whether a reference is real.
            //
            // The bounded coverage that remains is documented rather than
            // papered over: **an offloaded value is searchable only within its
            // preview.** Both sides of the search read the span through this
            // one function, so the index and the answer still agree exactly —
            // there is no wrong result, only a bounded one. Covering the full
            // text would mean reading the payload file at seal AND at every
            // verification, which is why it is roadmap work rather than a
            // line here.
            Value::Object(map) => map
                .iter()
                .filter(|(key, _)| key.as_str() != payload::PAYLOAD_KEY)
                .for_each(|(_, item)| collect(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for value in span.attributes.values() {
        collect(value, &mut out);
    }
    for event in &span.events {
        out.push(&event.name);
        for value in event.attributes.values() {
            collect(value, &mut out);
        }
    }
    out
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
    if !span.tenant.is_empty() {
        attributes.insert(IDX_TENANT.to_owned(), span.tenant.clone());
    }
    // Content is carried unescaped and separately from `attributes`, whose
    // values are canonical JSON. See `RecordInput::content`.
    let content = content_strs(span)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<String>>();
    Ok(segment::RecordInput::new(
        span.start_time_ns,
        span.trace_id.clone(),
        attributes,
        serde_json::to_vec(span)?,
    )
    .with_content(content))
}

fn record_to_span(record: &segment::Record) -> Result<Span> {
    // Identity comes off the wire and the disk under the reserved `$tenant`
    // key (see [`Span::tenant`]). A record written before tenancy carried a
    // bare `tenant` at most as client data, and it decodes back into
    // [`Span::extra`] untouched — never into identity — so no repair pass is
    // needed and none exists to be unsound.
    Ok(serde_json::from_slice(record.payload())?)
}

/// Whether `tenant` is admissible as a tenant identity: `[a-z0-9]` first,
/// then up to 63 more of `[a-z0-9._-]`. Lowercase-only by fiat — an identity
/// that needs canonicalizing will one day be compared uncanonicalized.
pub fn valid_tenant(tenant: &str) -> bool {
    let bytes = tenant.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if bytes.len() > 64 {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    })
}

impl Segment {
    fn record_count(&self) -> usize {
        self.seg.len()
    }

    /// Identity a persisted rollup must match to describe THIS segment.
    ///
    /// Segment files are immutable once renamed into place, so these four
    /// numbers cannot drift under a valid sidecar. What they defend against
    /// is a sidecar left behind by a different file that later took the same
    /// name, and a sidecar half-written by an older build.
    /// `pricing_fingerprint` is the store's, not the segment's: the segment
    /// file says nothing about rates, and it is the counters in the sidecar —
    /// not the spans — that were computed under a particular table.
    fn rollup_binding(&self, pricing_fingerprint: u64) -> rollup_file::Binding {
        let (min_start_ns, max_start_ns) = self.seg.timestamp_range();
        rollup_file::Binding {
            segment_bytes: self.bytes,
            record_count: self.seg.len() as u64,
            min_start_ns,
            max_start_ns,
            pricing_fingerprint,
        }
    }

    fn contains_key(&self, tenant: &str, trace_id: &str, span_id: &str) -> Result<bool> {
        // The trace index is keyed by trace text alone and may hold another
        // tenant's spans under the same id — it accelerates, the decoded
        // span decides (invariant 7).
        for span in self.trace_spans(trace_id)? {
            if span.span_id == span_id && span.tenant == tenant {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The hash of every primary key this segment holds, built once per
    /// segment lifetime: the prefilter in front of [`Self::contains_key`].
    ///
    /// Loaded from the rollup sidecar when one matches this generation of the
    /// file; a segment without one — a pre-sidecar store, a sidecar lost to a
    /// crash — is decoded once and heals its sidecar in passing, exactly as
    /// `Store::segment_rollup` does. The decode's spans are dropped: what is
    /// retained is eight bytes per distinct key, never payloads.
    ///
    /// The contract callers lean on: a key present in this segment is
    /// GUARANTEED to have its hash here, because both sources fold the hash of
    /// every stored span. A membership miss is therefore proof of absence; a
    /// hit proves nothing and must be confirmed against the records.
    ///
    /// Takes the store's pricing even though a hash set cannot depend on it:
    /// this path WRITES the sidecar when it has to build one, and a sidecar
    /// bound to the wrong rate table is one the analytics path will reject and
    /// rebuild — which would then be rejected here, each overwriting the
    /// other's file on every call.
    fn key_hashes(
        &self,
        pricing: &crate::pricing::Pricing,
    ) -> Result<&std::collections::HashSet<u64>> {
        if let Some(hashes) = self.key_hashes.get() {
            return Ok(hashes);
        }
        let binding = self.rollup_binding(pricing.fingerprint());
        let rollup = match rollup_file::load(&self.path, binding) {
            Some(rollup) => rollup,
            None => {
                let rollup = analytics::SegmentRollup::build(&self.spans_parsed()?, pricing);
                let _ = rollup_file::store(&self.path, binding, &rollup);
                rollup
            }
        };
        // Two readers may race the build; first `set` wins and both answers
        // are identical, so the loser's work is discarded, not wrong.
        Ok(self.key_hashes.get_or_init(|| rollup.key_hashes))
    }

    /// Full parse — the rewrite/inspection path, never the query path.
    fn spans_parsed(&self) -> Result<Vec<Span>> {
        self.spans_parsed_in_window(None, None)
    }

    /// The spans whose start time falls in the window, decoding ONLY the
    /// records the window covers.
    ///
    /// Records are stored in ascending start-time order and a span's record
    /// timestamp IS its `start_time_ns`, so the window is a contiguous ordinal
    /// range that [`segment::Segment::ordinal_range_for_window`] finds by
    /// binary search. That is what keeps a dashboard's "last hour" from paying
    /// for the whole corpus: the aggregation path decodes a slice, not a file.
    fn spans_parsed_in_window(&self, since: Option<u64>, until: Option<u64>) -> Result<Vec<Span>> {
        let range = self
            .seg
            .ordinal_range_for_window(since, until)
            .map_err(segment_error)?;
        let mut spans = Vec::with_capacity(range.len());
        for ordinal in range {
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
/// Both the trace and span halves of the (tenant, trace_id, span_id) primary
/// key must be non-empty at the engine boundary, not just at the HTTP
/// surfaces: distinct spans sharing an empty id would silently collapse into
/// one upserted key for any library consumer too. The tenant half is the one
/// component that MAY be empty — empty IS the default tenant — but a
/// non-empty tenant must satisfy [`valid_tenant`], for the same reason both
/// checks live here at all.
fn validate_span(span: &Span) -> Result<()> {
    if span.trace_id.is_empty() {
        return Err(Error::InvalidSpan("trace_id is empty"));
    }
    if span.span_id.is_empty() {
        return Err(Error::InvalidSpan("span_id is empty"));
    }
    if !span.tenant.is_empty() && !valid_tenant(&span.tenant) {
        return Err(Error::InvalidSpan(
            "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
        ));
    }
    Ok(())
}

/// (tenant, trace_id, span_id) is the span's PRIMARY KEY: re-ingesting an
/// existing key replaces the buffered version in place — retries are
/// idempotent and never create a second acknowledged copy — and two tenants
/// sharing a trace id can never upsert over each other.
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
    index: std::collections::HashMap<(String, String, String), usize>,
    /// Spans upserted since the last seal, counting replacements. `spans.len()`
    /// deliberately does not: an update to a buffered key leaves the record
    /// count untouched while still costing a log record, so the flush policy
    /// needs both numbers (see [`Config::flush_spans`]).
    upserts: usize,
    /// When the buffer last went from empty to holding something — the age
    /// [`Config::max_buffer_age`] bounds. `None` exactly when the buffer is
    /// empty. Approximate at the edges by design: replay and seal survivors
    /// restart the clock at the moment of restore or reconcile, which can only
    /// understate age by one seal's duration — an error measured in seconds
    /// against a bound measured in minutes, in the direction of a later, not
    /// earlier, seal.
    oldest_at: Option<Instant>,
}

impl WriteBuffer {
    /// Inserts or replaces `span`, returning the handle now holding it.
    ///
    /// The handle is returned so ingest can publish the same allocation to the
    /// tail ring (see [`crate::tail`]) instead of copying the span a second
    /// time. It is a `Arc::clone`, so the ring costs one pointer per entry and
    /// keeps the span alive after a seal evicts it from here — which is exactly
    /// what a tail wants to keep showing.
    fn upsert(&mut self, span: Span) -> std::sync::Arc<Span> {
        let key = (
            span.tenant.clone(),
            span.trace_id.clone(),
            span.span_id.clone(),
        );
        self.upserts += 1;
        if self.spans.is_empty() {
            self.oldest_at = Some(Instant::now());
        }
        // A fresh allocation every time, including for a replacement: the old
        // handle may be held by a seal in flight, and the seal decides what to
        // evict by comparing handles. Mutating in place would make the newer
        // version indistinguishable from the sealed one and get it evicted.
        let span = std::sync::Arc::new(span);
        let handle = std::sync::Arc::clone(&span);
        match self.index.get(&key) {
            Some(&position) => self.spans[position] = span,
            None => {
                self.index.insert(key, self.spans.len());
                self.spans.push(span);
            }
        }
        handle
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn contains_key(&self, tenant: &str, trace_id: &str, span_id: &str) -> bool {
        self.index
            .contains_key(&(tenant.to_owned(), trace_id.to_owned(), span_id.to_owned()))
    }

    /// Adopts `spans` as the whole buffer, rebuilding the position index.
    ///
    /// The index maps a primary key to a POSITION, so adopting a vector
    /// without rebuilding it would leave `upsert` overwriting the wrong span.
    fn restore(&mut self, spans: Vec<std::sync::Arc<Span>>) {
        self.spans = spans;
        self.reindex();
        // Replayed spans' true arrival times died with the previous process;
        // the age clock restarts at restore, which can only delay their seal
        // by one `max_buffer_age`.
        self.oldest_at = (!self.spans.is_empty()).then(Instant::now);
    }

    fn retain(&mut self, keep: impl Fn(&Span) -> bool) {
        self.spans.retain(|span| keep(span));
        self.reindex();
        // Survivors were already here, so their age stands; only emptiness
        // resets the clock.
        if self.spans.is_empty() {
            self.oldest_at = None;
        }
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
        // Survivors arrived while the seal was writing, so "now" overstates
        // their arrival by at most that write — the age clock may run late by
        // seconds against a bound of minutes, never early.
        self.oldest_at = (!self.spans.is_empty()).then(Instant::now);
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (position, span) in self.spans.iter().enumerate() {
            self.index.insert(
                (
                    span.tenant.clone(),
                    span.trace_id.clone(),
                    span.span_id.clone(),
                ),
                position,
            );
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
    /// The log's stamp position at the drain, captured under the writer lock
    /// so no append can interleave. Everything at or before it is in the
    /// drained spans — which is exactly what a checkpoint built on this seal
    /// may record as its `folded_through`.
    position: Option<generation::FoldedThrough>,
}

/// What a seal accomplished: whether a segment was published, and — when the
/// store keeps a log — the fold point a checkpoint may claim for it.
struct SealOutcome {
    published: bool,
    folded: Option<generation::FoldedThrough>,
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

/// A cached rollup, together with the identity of the segment it was built
/// from.
///
/// The binding is not decoration. The cache is keyed by PATH, and a path is
/// not an identity: TTL expiry rewrites a segment in place, so the same name
/// can hold different bytes over time — and because a reader PINS the segment
/// list rather than holding the lock, a fold can be decoding a segment's old
/// bytes through its own descriptor while the cache already holds the rollup
/// for the rewritten ones. Serving that hit mixes two generations of the same
/// segment: the survivors come from the old bytes and the supersede prefilter
/// from the new rollup, and a span that both segments still physically
/// contain gets counted twice.
///
/// The sidecar path never had this problem — `rollup_file::load` checks the
/// binding — so the fix is to give the in-memory path the same check rather
/// than a different one.
type CachedRollup = (
    rollup_file::Binding,
    std::sync::Arc<analytics::SegmentRollup>,
);

/// A segment just written to disk, plus the analytics rollup that was built
/// in order to persist its sidecar.
///
/// The rollup travels with the segment because the only place it can be had
/// for free is where the spans already are. Every other place has to decode
/// the segment back out of the file it was just written from.
struct Sealed {
    segment: Segment,
    rollup: std::sync::Arc<analytics::SegmentRollup>,
}

/// An erasure subject at request time: the span keys it covers, and the
/// payload references those spans carry.
type ResolvedSubject = (Vec<(String, String, String)>, Vec<String>);

/// What one erasure's purge removed, counted physically: superseded versions
/// of an erased key held the bytes too, so they count and they go.
#[derive(Debug, Default)]
struct SubjectPurge {
    /// Physical records removed across buffer, log and segments.
    removed: usize,
    /// Records rewritten with a redacted payload reference.
    redacted: usize,
    /// Every key a removed or redacted record carried — the tail veil's
    /// coverage, and the union the annotation drop tests against.
    keys: std::collections::HashSet<(String, String, String)>,
    /// Every payload reference a REMOVED record carried, before redaction.
    payload_refs: std::collections::HashSet<String>,
}

/// One tenant's usage row, from [`Store::tenant_usage`].
#[derive(Clone, Debug, Serialize)]
pub struct TenantUsage {
    /// The tenant; empty is the default tenant.
    pub tenant: String,
    /// Logical spans currently visible (LWW-resolved, superseded versions
    /// excluded — this is what the tenant sees, not what disk holds).
    pub spans: u64,
    /// Distinct traces among them.
    pub traces: u64,
    /// Serialized size of those spans, approximately — inline content only.
    pub bytes_approx: u64,
    /// Bytes of distinct offloaded payloads the tenant's spans reference,
    /// from the reference objects' recorded sizes. A blob shared across
    /// tenants counts for EVERY referencing tenant: for a quota question,
    /// each of them is holding the store to those bytes.
    pub payload_bytes_approx: u64,
    /// Earliest span start.
    pub first_start_ns: u64,
    /// Latest span end.
    pub last_end_ns: u64,
}

/// The recorded byte size of `reference` as `span` carries it — reference
/// objects record `bytes` at offload time, so accounting never opens files.
fn payload_ref_bytes(span: &Span, reference: &str) -> u64 {
    let from = |attributes: &Map<String, Value>| {
        attributes.values().find_map(|value| {
            (value.get(payload::PAYLOAD_KEY).and_then(Value::as_str) == Some(reference))
                .then(|| value.get("bytes").and_then(Value::as_u64))
                .flatten()
        })
    };
    from(&span.attributes)
        .or_else(|| span.events.iter().find_map(|event| from(&event.attributes)))
        .unwrap_or(0)
}

/// Wall clock in Unix nanoseconds, saturating at zero before the epoch.
fn unix_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64)
}

/// The per-tenant expiry policy, resolved to absolute cutoffs.
///
/// One number cannot express "tenant A keeps a day, tenant B keeps a month,
/// tenant C keeps everything" — and the segment fast paths need CONSERVATIVE
/// bounds over the whole policy, not a bound over whichever tenants happen to
/// be configured. Getting the retire-whole bound wrong deletes an unswept
/// tenant's segment outright, which is why the bounds are computed here, once,
/// instead of at each use site.
#[derive(Debug)]
struct ExpiryCutoffs {
    /// Cutoff for tenants without an override; `None` means such tenants
    /// never expire.
    default: Option<u64>,
    /// Per-tenant overrides. `None` is an EXPLICIT exemption — a tenant
    /// configured with `--tenant-ttl acme=0` keeps everything even when a
    /// global window exists. Dropping the zero entry instead let the
    /// tenant fall through to the global cutoff, which deleted exactly the
    /// data the operator had just exempted.
    tenants: std::collections::HashMap<String, Option<u64>>,
}

impl ExpiryCutoffs {
    /// One cutoff for everyone — [`Store::expire_before`]'s contract.
    fn single(cutoff_ns: u64) -> Self {
        Self {
            default: Some(cutoff_ns),
            tenants: std::collections::HashMap::new(),
        }
    }

    /// The configured policy as absolute cutoffs, or `None` when no window
    /// is configured anywhere (retention wholly disabled). A zero GLOBAL
    /// TTL is documented as "disabled"; a zero PER-TENANT TTL is that
    /// tenant's exemption from the global window.
    fn from_config(config: &Config, now_ns: u64) -> Option<Self> {
        let cutoff = |ttl_seconds: u64| {
            (ttl_seconds > 0)
                .then(|| now_ns.saturating_sub(ttl_seconds.saturating_mul(1_000_000_000)))
        };
        let default = config.ttl_seconds.and_then(cutoff);
        let tenants: std::collections::HashMap<String, Option<u64>> = config
            .tenant_ttl_seconds
            .iter()
            .map(|(tenant, ttl)| (tenant.clone(), cutoff(*ttl)))
            .collect();
        if default.is_none() && tenants.values().all(Option::is_none) {
            return None;
        }
        Some(Self { default, tenants })
    }

    /// This tenant's cutoff: its override (where `0` means NEVER), else
    /// the default, else never.
    fn cutoff_for(&self, tenant: &str) -> Option<u64> {
        match self.tenants.get(tenant) {
            Some(exempt_or_cutoff) => *exempt_or_cutoff,
            None => self.default,
        }
    }

    /// The latest instant any configured window reaches — a span ending at
    /// or after this expires under NO policy, so a segment entirely newer
    /// is skipped without a decode.
    fn latest(&self) -> u64 {
        self.tenants
            .values()
            .flatten()
            .copied()
            .chain(self.default)
            .max()
            .unwrap_or(0)
    }

    /// The earliest instant EVERY span is bound by, or `None` when no such
    /// bound exists. It requires the default window AND no exempt tenant:
    /// a tenant with no cutoff at all — unlisted with no default, or
    /// explicitly exempted with `0` — makes any whole-segment deletion
    /// unsound, because the segment may hold that tenant's spans.
    fn retire_bound(&self) -> Option<u64> {
        let default = self.default?;
        if self.tenants.values().any(Option::is_none) {
            return None;
        }
        Some(
            self.tenants
                .values()
                .flatten()
                .copied()
                .chain(std::iter::once(default))
                .min()
                .expect("at least the default"),
        )
    }

    /// The earliest configured cutoff — the payload sweep's mtime gate.
    /// Liveness and touch immunity carry payload safety; this bound only
    /// protects brand-new files, so the most conservative instant costs at
    /// most a lingering orphan.
    fn earliest(&self) -> u64 {
        self.tenants
            .values()
            .flatten()
            .copied()
            .chain(self.default)
            .min()
            .unwrap_or(0)
    }
}

/// A durable span store backed by sorted JSON-lines segment files.
pub struct Store {
    /// The store directory: segments, the annotation log, and payload files
    /// live here directly, alongside the lock, the write-ahead log, `CURRENT`,
    /// and the `generations/` and `pins/` metadata. A generation references
    /// the engine files in place rather than owning a separate copy of them.
    directory: PathBuf,
    /// The generation `CURRENT` names. Advanced by [`Store::checkpoint`]
    /// after its `CURRENT` rename is durable, never before.
    live_generation: AtomicU64,
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
    // Both rewriters take it too, for different spans of time. Expiry holds
    // it for its whole deletion: a seal that drained before the deletion ran
    // would otherwise publish its segment afterwards and resurrect exactly the
    // spans expiry just removed from the buffer, the log, and every segment it
    // knew about. Compaction holds it only while it chooses a run and claims
    // the ids for its outputs, because a seal holding an unpublished lower id
    // would sort before merged output that is strictly older.
    //
    // Ingest mostly keeps flowing while a rewriter holds it, because a seal
    // that cannot take it coalesces into the next one. Two paths wait instead,
    // and both are worth knowing about: [`Durability::Flushed`] must seal
    // before it acknowledges, and any mode blocks once the buffer has reached
    // four times `flush_spans` (see `seal_must_not_be_skipped`), where waiting
    // is the backpressure. Under either, an ingest waits out a running
    // deletion — but only the microseconds of a compaction's claim.
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
    rollups: Mutex<std::collections::HashMap<PathBuf, CachedRollup>>,
    recent_payloads: payload::TouchRegistry,
    // The live tail's admission ring. It takes NO other lock and is never
    // taken while another engine lock is held — ingest publishes to it after
    // releasing the writer lock — so it sits outside the ordering above
    // entirely rather than at the bottom of it.
    tail: tail::TailChannel,
    annotations: annotations::AnnotationLog,
    /// The eval log: datasets, versions, examples, experiments, runs — a
    /// manifested recovery domain like the annotation log. Its mutex is
    /// leaf-level; every mutation additionally holds the erasure gate's read
    /// half, because a tenant erasure rewrites this log inside its barrier.
    evals: evals::EvalLog,
    /// The tombstone log: every erasure requested and settled, plus the mask
    /// over the pending ones that every read path consults. See [`erasure`].
    erasures: erasure::ErasureLog,
    /// The erasure admission gate. Ingest and annotation admission hold it in
    /// READ mode from their mask load through their store mutation; `begin`
    /// and settle hold it in WRITE mode while they move the mask. That
    /// span-of-time exclusion is what makes a one-shot mask application
    /// sound at the transition: a batch is wholly before the erasure (the
    /// purge finds its spans and payload files in the store) or wholly after
    /// (the mask governs its drops, its offload writes, its upserts) — never
    /// astride it. Outermost in the lock order: taken before writer,
    /// erasures, or annotations, and never while holding any of them.
    erasure_gate: std::sync::RwLock<()>,
    next_segment: AtomicU64,
    /// Present unless durability is [`Durability::Buffered`]. Guards the gap
    /// between acknowledging a write and sealing it into a segment.
    wal: Option<wal::Wal>,
    /// Set by an analytics fold that had to take the exact path because a
    /// segment's keys are shadowed by a NEWER SEGMENT, and consumed by
    /// [`Store::maintain_buffer`]. A latch rather than a count: one shadowed
    /// key disqualifies a segment's rollup exactly as thoroughly as a
    /// thousand, and a count over persisted keys would cost the memory the
    /// digest index rewrite exists to avoid.
    shadowing_observed: std::sync::atomic::AtomicBool,
    /// The shadow pass's clock: when the next pass may run, and the current
    /// futility backoff. See [`Store::finish_shadow_pass`].
    shadow_pass: Mutex<ShadowPassClock>,
    metrics: metrics::Metrics,
    _directory_lock: DirectoryLock,
}

impl Store {
    /// Restores a store from a backup directory `staged` into `root`, then
    /// opens it. `root` must not have a live store against it.
    ///
    /// `staged` is a copy of a pin: an engine file set plus its manifest. The
    /// backup is verified before anything is swapped, and the swap commits at
    /// one `CURRENT` rename, so a failed or interrupted restore leaves the
    /// prior store (or nothing) rather than a blend. This is the inverse of
    /// `pin` + copy.
    pub fn restore(
        root: impl AsRef<Path>,
        staged: impl AsRef<Path>,
        config: Config,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let lock = DirectoryLock::acquire(root.join(LOCK_FILE_NAME))?;
        generation::install_staged(&root, staged.as_ref())?;
        drop(lock);
        Self::open(root, config)
    }

    /// Opens or creates a store in `path`.
    ///
    /// Only one live `Store` may own a directory. Opening also removes orphaned
    /// temporary segment files left by an interrupted write.
    ///
    /// A directory from before the generation layout is migrated at first
    /// open — engine files move under `engine/`, the log is rewritten in
    /// stamped framing, and generation one is published — after which
    /// `CURRENT` names the live generation and every open loads through it.
    /// The migration is one-way and resumable: it commits by publishing
    /// `CURRENT`, so a crash anywhere before that leaves a directory the next
    /// open finishes migrating, and a crash after it leaves a migrated store.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;

        let lock_path = root.join(LOCK_FILE_NAME);
        let directory_lock = DirectoryLock::acquire(lock_path)?;

        let opened = (|| {
            let directory = root;
            let live_generation = match generation::read_current(&directory)? {
                Some(live) => live,
                None => migrate_to_generations(&directory)?,
            };
            let manifest =
                generation::load_manifest(&generation::manifest_path(&directory, live_generation))?;

            remove_orphan_temps(&directory)?;
            // Interrupted compaction rewrites are finished from their
            // supersede markers BEFORE loading: recovery follows the journal,
            // never content — content-based duplicate healing silently
            // destroyed legitimately re-ingested identical spans (found in
            // review: acknowledged duplicate cardinality must survive
            // restart).
            recover_supersede_markers(&directory)?;
            let mut segments = load_segments(&directory)?;
            // A sidecar whose segment is gone can never be read again — the
            // binding check ties a rollup to one exact segment — so it is
            // pure waste. `unlink_segment` removes them with their segments;
            // this catches the ones a crash stranded between the two
            // unlinks, so they cannot accumulate across restarts.
            remove_orphan_rollups(&directory, &segments)?;
            segments.sort_by(|left, right| left.path.cmp(&right.path));

            // Replay BEFORE accepting new writes. Records are append-ordered
            // and upserted in that order, so the newest version of a
            // re-ingested key wins exactly as it did before the crash. Frames
            // at or before the live generation's `folded_through` are already
            // inside its files and are discarded, trimmed or not — which is
            // what makes a published deletion durable against a log that was
            // never rolled over.
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
                // The handle is discarded rather than published to the tail:
                // these are spans admitted before the restart, and the tail is
                // a live surface. Replaying them would show a reconnecting
                // client the last buffer's worth of history as though it had
                // just arrived.
                let recovered = wal::Wal::recover(&directory, manifest.folded_through, |span| {
                    buffer.upsert(span);
                })?;
                // New stamps must land strictly after BOTH everything the log
                // holds and everything any manifest folded — a stamp that
                // sorted at or before either would be discarded on the next
                // replay as if it had been folded, which is data loss. The
                // epoch likewise resumes at its high-water mark: a checkpoint
                // whose `CURRENT` rename did not survive leaves frames stamped
                // under the generation that never went live.
                let epoch = live_generation.max(recovered.highest.epoch);
                let resume_after = recovered
                    .highest
                    .sequence
                    .max(manifest.folded_through.sequence);
                Some(wal::Wal::open(
                    &directory,
                    config.wal_commit_window,
                    epoch,
                    resume_after,
                )?)
            };
            let next_segment = segments
                .iter()
                .filter_map(|segment| segment_number(&segment.path))
                .max()
                .map_or(0, |number| number.saturating_add(1));
            let tail_ring_spans = config.tail_ring_spans;
            let tail_ring_bytes = config.tail_ring_bytes;

            Ok(Self {
                annotations: annotations::AnnotationLog::open(&directory)?,
                evals: evals::EvalLog::open(&directory)?,
                // Replayed before serving: a pending erasure — one whose purge
                // a crash interrupted — masks its subject from the first query
                // this store answers, and stays masked until the resumed purge
                // settles it (see [`Store::resume_erasures`]).
                erasures: erasure::ErasureLog::open(&directory)?,
                erasure_gate: std::sync::RwLock::new(()),
                directory,
                live_generation: AtomicU64::new(live_generation),
                config,
                maintenance: Mutex::new(()),
                sealing: Mutex::new(()),
                writer: Mutex::new(buffer),
                segments: Mutex::new(segments),
                rollups: Mutex::new(std::collections::HashMap::new()),
                recent_payloads: payload::TouchRegistry::default(),
                tail: tail::TailChannel::new(tail_ring_spans, tail_ring_bytes),
                next_segment: AtomicU64::new(next_segment),
                wal,
                shadowing_observed: std::sync::atomic::AtomicBool::new(false),
                shadow_pass: Mutex::new(ShadowPassClock::default()),
                metrics: metrics::Metrics::default(),
                _directory_lock: directory_lock,
            })
        })();

        opened
    }

    /// Adds one span, automatically flushing when the configured threshold is
    /// reached.
    pub fn ingest(&self, span: Span) -> Result<Admission> {
        self.ingest_batch(vec![span])
    }

    /// Adds a batch of spans, automatically flushing when the configured
    /// threshold is reached. The batch is atomic with respect to validation:
    /// if any span is invalid, nothing from the batch is stored.
    ///
    /// The returned [`Admission`] says what actually happened: how many
    /// spans were stored, and how many a pending erasure suppressed. A
    /// suppressed span was acknowledged and deliberately not stored, and a
    /// caller reporting durability to ITS caller must not count it as
    /// durable.
    pub fn ingest_batch(&self, spans: Vec<Span>) -> Result<Admission> {
        if spans.is_empty() {
            return Ok(Admission::default());
        }
        for span in &spans {
            validate_span(span)?;
        }
        let sent = spans.len();
        let mut spans = spans;

        // ---- the erasure admission barrier -------------------------------
        // Everything from the mask load to the buffer upsert happens under
        // one read acquisition of the erasure gate, and `begin`/settle take
        // it in write mode. That span-of-time exclusion is the barrier's
        // whole guarantee: a batch either completes wholly BEFORE an erasure
        // begins (its spans and payload files are in the store, where the
        // purge finds and accounts for them) or begins wholly AFTER (the
        // mask governs every step: the drop below, the offload's file
        // writes, the admission). A one-shot mask check cannot say that — a
        // mask installed between the check and the offload's file write left
        // orphan payload bytes of a suppressed span that no record named,
        // and a mask installed between the check and the upsert was the
        // original pre-settle leak. The gate closes the transition, not just
        // the steady pending state.
        let gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        if let Some(mask) = &mask {
            // Covered spans are dropped BEFORE payload offloading, so a
            // suppressed span's oversized values never reach the filesystem.
            spans.retain(|span| !mask.covers_for_drop(span));
        }
        if let Some(threshold) = self.config.payload_threshold {
            // Offloading consults the mask too: content whose hash IS a
            // pending subject offloads to its redacted marker, never bytes.
            let masked = mask.as_deref().map(erasure::Mask::payload_subjects);
            for span in &mut spans {
                payload::offload_span(
                    &self.directory,
                    span,
                    threshold,
                    &self.recent_payloads,
                    masked,
                )?;
            }
        }
        if let Some(mask) = &mask {
            // A client can also SUPPLY reference objects verbatim — spans
            // round-tripped through export re-ingest them — so the redaction
            // of pending payload subjects runs against the post-offload
            // batch, where both shapes look the same.
            for reference in mask.payload_subjects() {
                for span in &mut spans {
                    erasure::redact_payload(span, reference);
                }
            }
            if spans.is_empty() {
                self.metrics.erasure_spans_suppressed.add(sent as u64);
                return Ok(Admission {
                    accepted: 0,
                    suppressed: sent,
                });
            }
        }

        self.admit(spans, sent, gate)
    }

    /// The acknowledgement path shared by both ingest surfaces.
    ///
    /// Ordering is the whole contract:
    /// 1. append the batch to the log and upsert it into the buffer, both
    ///    under the writer lock, so a concurrent seal cannot drain a buffer
    ///    that disagrees with the log;
    /// 2. release the lock;
    /// 3. fsync, and only then return;
    /// 4. seal, if the buffer has reached one of its bounds;
    /// 5. publish to the live tail, which is therefore a stream of
    ///    ACKNOWLEDGED admissions rather than of attempted ones.
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
    fn admit(
        &self,
        spans: Vec<Span>,
        sent: usize,
        gate: std::sync::RwLockReadGuard<'_, ()>,
    ) -> Result<Admission> {
        // The caller holds the erasure gate in read mode and already applied
        // the mask it loaded under it — drops, masked offloading, redaction.
        // The gate is what makes that one-shot application sound: `begin`
        // and settle take it in write mode, so the mask CANNOT move between
        // the caller's filter and the upserts below. It is held through the
        // writer-lock section and released before the fsync and any seal —
        // durability work needs no exclusion against an erasure beginning,
        // only the decision of what enters the store does.
        let mut pending_commit = None;
        let seal_now;
        let seal_must_wait;
        let admitted;
        let admitted_handles;
        // Encode the log frame BEFORE taking the writer lock. Serializing a
        // batch is pure CPU proportional to its size, and doing it under the
        // lock made every concurrent ingest wait for it. Only the file write
        // has to be inside the lock (below).
        let mut frame = match &self.wal {
            Some(_) => Some(self.metrics.wal_encode.time(|| wal::Wal::encode(&spans))?),
            None => None,
        };
        {
            let waited = Instant::now();
            let mut writer = self.lock_writer()?;
            self.metrics.writer_lock_wait.record(elapsed_nanos(&waited));
            if let (Some(log), Some(frame)) = (&self.wal, frame.as_mut()) {
                pending_commit = Some(
                    self.metrics
                        .wal_write
                        .time(|| log.append(frame, &self.metrics))?,
                );
            }
            admitted = spans.len() as u64;
            admitted_handles = self.metrics.buffer_upsert.time(|| {
                spans
                    .into_iter()
                    .map(|span| writer.upsert(span))
                    .collect::<Vec<_>>()
            });
            seal_now = self.should_flush(&writer);
            seal_must_wait = self.seal_must_not_be_skipped(&writer);
        }
        drop(gate);

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

        // Publish to the tail LAST, once this batch is acknowledged.
        //
        // "Admitted" means acknowledged, and the boundary is here: every `?`
        // above can still fail, and under `Durability::Wal` the fsync that
        // makes the batch survivable has only just returned. Publishing
        // earlier — which is what this did — let the tail show a span whose
        // ingest then returned an error, or which a crash a millisecond later
        // erased. A live view is allowed to be bounded and to admit gaps; it
        // is not allowed to show data the store never accepted.
        //
        // It stays outside the writer lock, so sequence numbers are assigned
        // in acknowledgement order rather than in write-buffer order. Under
        // concurrency those can differ, and acknowledgement order is the more
        // meaningful of the two: it is what a caller observed, and it is what
        // a replicated commit position would later refine into a total order.
        // The cursor only needs the numbers to be monotonic and gapless, which
        // assigning them at push guarantees however threads interleave.
        self.tail.publish(&admitted_handles);

        self.metrics.spans_admitted.add(admitted);
        self.metrics.batches_admitted.increment();
        let suppressed = sent.saturating_sub(admitted as usize);
        if suppressed > 0 {
            self.metrics.erasure_spans_suppressed.add(suppressed as u64);
        }
        Ok(Admission {
            accepted: admitted as usize,
            suppressed,
        })
    }

    /// Per-stage ingest instrumentation. See [`metrics::Metrics`].
    pub fn metrics(&self) -> &metrics::Metrics {
        &self.metrics
    }

    /// What an acknowledged ingest currently guarantees.
    pub fn durability(&self) -> Durability {
        self.config.durability
    }

    /// A span's normalized LLM facts, with this store's pricing applied.
    ///
    /// **Every surface that reports a cost must extract facts through here**,
    /// not through [`semconv::facts`] directly, or it reports metered cost
    /// only and disagrees with the surfaces that do. Extraction stays a pure
    /// function of the attributes; this is the one place the store's
    /// configuration joins it. Callers that only want the session id can use
    /// `semconv::facts` — pricing cannot change that answer.
    pub(crate) fn facts(&self, span: &Span) -> semconv::LlmFacts {
        semconv::facts(&span.attributes).priced(&self.config.pricing)
    }

    /// The per-model rates this store derives unmetered costs at.
    pub(crate) fn pricing(&self) -> &crate::pricing::Pricing {
        &self.config.pricing
    }

    /// The pricing table's digest, which a rollup sidecar must agree with.
    pub(crate) fn pricing_fingerprint(&self) -> u64 {
        self.config.pricing.fingerprint()
    }

    /// Returns the current number of spans buffered in memory.
    pub fn buffered_span_count(&self) -> usize {
        self.writer.lock().map_or(0, |writer| writer.len())
    }

    /// The newest tail position — where a subscriber wanting only future spans
    /// begins.
    pub fn tail_head(&self) -> Option<tail::TailCursor> {
        self.tail.head()
    }

    /// The live tail's residency as `(spans, bytes, max_spans, max_bytes)`.
    ///
    /// Exposed because the ring is the only structure in the engine that holds
    /// whole spans for an unbounded time, so an operator needs to be able to
    /// see it rather than deduce it from RSS.
    pub fn tail_usage(&self) -> Option<(usize, usize, usize, usize)> {
        self.tail.usage()
    }

    /// Waits up to `timeout` for spans admitted after `cursor` that match
    /// `filter`.
    ///
    /// This is the live tail, and it is ordered by ADMISSION rather than event
    /// time — see [`tail`] for why that distinction is the whole point.
    ///
    /// **`since_ns` and `until_ns` are rejected, not ignored.** An event-time
    /// window cannot be honoured on an admission-ordered stream, and applying
    /// one anyway is the original bug: a span that started before the window
    /// but landed inside it is dropped, the cursor advances past it, and it
    /// never appears. Documenting the field as "ignored" while passing the
    /// whole filter to `span_matches` reproduced exactly that for every library
    /// caller, which is why this is a type-checked refusal now rather than a
    /// sentence in a doc comment.
    ///
    /// Takes only the ring's lock, so an idle subscriber parked here blocks
    /// neither ingest nor any other query.
    pub fn tail_after(
        &self,
        cursor: Option<tail::TailCursor>,
        backfill: usize,
        limit: usize,
        filter: &SpanFilter,
        timeout: std::time::Duration,
    ) -> Result<tail::TailRead> {
        if filter.since_ns.is_some() || filter.until_ns.is_some() {
            return Err(Error::UnsupportedFilter(
                "a tail streams in admission order, so since_ns/until_ns cannot \
                 be honoured; use Store::query for an event-time window",
            ));
        }
        // Content search works on the tail without any index: the ring holds
        // whole spans, so the query runs against the text in memory rather than
        // against the postings a segment would have needed at seal time.
        let content = filter.content.as_deref().map(content::Query::new);
        // The ring's veils hide what settled erasures covered; the mask hides
        // what a PENDING one covers, for the same reason every other read
        // path consults it.
        let mask = self.erasure_mask();
        Ok(self.tail.wait(cursor, backfill, limit, timeout, &|span| {
            mask.as_deref().map_or(true, |mask| !mask.covers(span))
                && span_matches(span, filter, content.as_ref())
        }))
    }

    /// Persists every currently buffered span as one sorted segment.
    ///
    /// Synchronous: when this returns, the spans that were buffered when it
    /// was called are in a segment on disk. Spans ingested by other threads
    /// WHILE it runs may still be buffered afterwards — they arrived after the
    /// snapshot this call sealed, and the next seal takes them.
    pub fn flush(&self) -> Result<()> {
        self.seal(SealWait::ForPermit).map(|_| ())
    }

    /// Returns all spans belonging to `trace_id`, ordered by start time.
    ///
    /// The writer and segment locks are held together while constructing the
    /// combined view, so a concurrent flush cannot move spans between halves
    /// of the snapshot and make them temporarily disappear.
    pub fn get_trace(&self, trace_id: &str) -> Result<Vec<Span>> {
        self.get_trace_in(None, trace_id)
    }

    /// [`Self::get_trace`] under a tenant scope. `None` is the operator view
    /// — every tenant's spans under this trace id, in one response — and
    /// `Some(tenant)` is that tenant's trace alone, which is what a bound
    /// credential always gets and what erasure subject resolution always
    /// uses: a subject resolved without the scope could record another
    /// tenant's newer same-id span and miss its own.
    pub fn get_trace_in(&self, tenant: Option<&str>, trace_id: &str) -> Result<Vec<Span>> {
        let mask = self.erasure_mask();
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut result = Vec::new();

        let in_scope = |span: &Span| tenant.map_or(true, |tenant| span.tenant.as_str() == tenant);
        // (tenant, trace_id, span_id) is the span's primary key: the newest
        // ingested version wins. Segments are visited oldest-first so later
        // versions overwrite, and the write buffer overwrites everything.
        // The LWW map is keyed by (tenant, span_id) — trace fixed by the
        // argument — so two tenants sharing an id never collapse into one
        // entry even in the operator view.
        let mut latest: std::collections::HashMap<(String, String), Span> =
            std::collections::HashMap::new();
        for segment in segments.iter() {
            for span in segment.trace_spans(trace_id)? {
                if in_scope(&span) {
                    latest.insert((span.tenant.clone(), span.span_id.clone()), span);
                }
            }
        }
        for span in writer.spans.iter() {
            if span.trace_id == trace_id && in_scope(span) {
                latest.insert(
                    (span.tenant.clone(), span.span_id.clone()),
                    Span::clone(span),
                );
            }
        }
        result.extend(
            latest
                .into_values()
                .filter(|span| mask.as_deref().map_or(true, |mask| !mask.covers(span))),
        );

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
        let mask = self.erasure_mask();
        // Lock order: writer before segments (see Store field docs).
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        attribute_union_view(&writer, &segments, keys, values, mask.as_deref())
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
            let spans = self.resolve_session_spans(filter.tenant.as_deref(), session_id)?;
            return Ok(narrow_session_spans(spans, filter, cursor));
        }
        let mask = self.erasure_mask();
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        query_view(
            &writer,
            &segments,
            &self.metrics,
            self.pricing(),
            filter,
            cursor,
            mask.as_deref(),
        )
    }

    /// [`Self::query_after`], additionally reporting what the query cost.
    ///
    /// The timer covers only the engine: lock acquisition, segment selection
    /// and decoding. Serializing the answer is the caller's time, and folding
    /// it in here would make the reported figure depend on how many bytes the
    /// client asked for rather than on how much of the store was read.
    pub fn query_costed(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
    ) -> Result<(Vec<Span>, QueryCost)> {
        let started = std::time::Instant::now();
        let mut cost = QueryCost::default();
        let spans = if let Some(session_id) = &filter.session {
            let spans = self.resolve_session_spans(filter.tenant.as_deref(), session_id)?;
            narrow_session_spans(spans, filter, cursor)
        } else {
            let mask = self.erasure_mask();
            let writer = self.lock_writer()?;
            let segments = self.lock_segments()?;
            query_view_costed(
                &writer,
                &segments,
                &self.metrics,
                self.pricing(),
                filter,
                cursor,
                mask.as_deref(),
                &mut cost,
            )?
        };
        cost.elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        Ok((spans, cost))
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
        let mask = self.erasure_mask();
        // Lock order: writer before segments (see Store field docs).
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        let mut buffer = WriteBuffer::default();
        buffer.restore(writer.spans.clone());
        Ok(SnapshotView {
            buffer,
            segments: segments.clone(),
            mask,
            metrics: &self.metrics,
            pricing: std::sync::Arc::clone(&self.config.pricing),
        })
    }

    /// The pending-erasure mask, or `None` when nothing is pending — the
    /// common case, and one `Arc` clone when not.
    fn erasure_mask(&self) -> Option<std::sync::Arc<erasure::Mask>> {
        self.erasures.mask()
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
    /// The pending-erasure mask as it stood when the view was taken. A view
    /// is one instant of the store, and what was invisible at that instant
    /// stays invisible for the view's whole life.
    mask: Option<std::sync::Arc<erasure::Mask>>,
    /// Reads through a view are real reads and are counted as such. Borrowing
    /// the store's counters is also what keeps a view from outliving the store
    /// whose files it pins.
    metrics: &'a metrics::Metrics,
    /// The rate table in force when the view was taken. A view's reads build
    /// and heal rollup sidecars like any other, so they must bind them to the
    /// same pricing the store does.
    pricing: std::sync::Arc<crate::pricing::Pricing>,
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
            let spans = analytics::resolve_session_spans_in(
                &self.buffer,
                &self.segments,
                filter.tenant.as_deref(),
                session_id,
                self.mask.as_deref(),
            )?;
            return Ok(narrow_session_spans(spans, filter, cursor));
        }
        query_view(
            &self.buffer,
            &self.segments,
            self.metrics,
            &self.pricing,
            filter,
            cursor,
            self.mask.as_deref(),
        )
    }

    /// Folds every matching span through `visit`, reporting what it cost.
    ///
    /// A view holds no engine lock, so a scan of the whole corpus runs
    /// alongside ingest instead of blocking it. See [`Store::fold_spans`].
    pub(crate) fn fold(
        &self,
        filter: &SpanFilter,
        cost: &mut QueryCost,
        visit: &mut impl FnMut(&Span),
    ) -> Result<()> {
        fold_view(
            &self.buffer,
            &self.segments,
            self.metrics,
            &self.pricing,
            filter,
            self.mask.as_deref(),
            cost,
            visit,
        )
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
    let content = filter.content.as_deref().map(content::Query::new);
    spans.retain(|span| {
        span_matches(span, filter, content.as_ref())
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
    mask: Option<&erasure::Mask>,
) -> Result<Vec<Span>> {
    let visible = |span: &Span| mask.map_or(true, |mask| !mask.covers(span));
    let matches = |span: &Span| {
        keys.iter().any(|key| {
            span.attributes
                .get(*key)
                .is_some_and(|held| values.iter().any(|value| held == value))
        })
    };

    let mut result: Vec<Span> = Vec::new();
    let mut claimed: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    // The buffer holds the newest version of anything it carries.
    for span in buffer.spans.iter() {
        if visible(span) && matches(span) {
            claimed.insert((
                span.tenant.clone(),
                span.trace_id.clone(),
                span.span_id.clone(),
            ));
            result.push(Span::clone(span));
        }
    }
    // Any key present in the buffer supersedes every segment copy, even
    // one the predicate does not select in its buffered version.
    for key in buffer.index.keys() {
        claimed.insert(key.clone());
    }
    // Newest segment first, so the first version claimed for a key wins.
    for segment in segments.iter().rev() {
        let seg = &segment.seg;
        let mut offsets: Vec<u64> = Vec::new();
        for key in keys {
            for value in values {
                // Same superset rule as `select_probe`: a session id stored as
                // a number under one convention and a string under another
                // must still return the session whole.
                offsets.extend_from_slice(&attribute_candidates(seg, key, value));
            }
        }
        offsets.sort_unstable();
        offsets.dedup();
        for offset in offsets {
            let record = seg.record_at_offset(offset).map_err(segment_error)?;
            let span = record_to_span(&record)?;
            // An index accelerates a filter, it never changes it.
            if !visible(&span) || !matches(&span) {
                continue;
            }
            if claimed.insert((
                span.tenant.clone(),
                span.trace_id.clone(),
                span.span_id.clone(),
            )) {
                result.push(span);
            }
        }
    }
    sort_spans(&mut result);
    Ok(result)
}

/// Whether any segment after `position` holds `span`'s primary key — the
/// last-write-wins test every read path applies to a segment candidate.
///
/// Prefiltered through each newer segment's key-hash set, ported from the
/// analytics fold's exact path: a set miss is proof the key was never written
/// there, so only a hit — a real supersede or an FNV collision — pays the
/// exact probe. Without the prefilter every emitted span probed the trace
/// index of every newer segment, and `contains_key` decodes the candidate's
/// whole trace in each one, so a store carrying many superseded versions (a
/// crash-recovered store before its first compaction is the canonical case)
/// answered in matches × segments × trace-width decodes.
///
/// The probe on a hit is not optional (see invariant 7, "an index accelerates
/// a filter; it never changes it"): dropping a span on hash membership alone
/// would let a collision delete a live row.
fn superseded_by_newer(
    segments: &[std::sync::Arc<Segment>],
    position: usize,
    span: &Span,
    metrics: &metrics::Metrics,
    pricing: &crate::pricing::Pricing,
) -> Result<bool> {
    let hash = analytics::key_hash(&span.tenant, &span.trace_id, &span.span_id);
    for newer in segments.iter().skip(position + 1) {
        if !newer.key_hashes(pricing)?.contains(&hash) {
            continue;
        }
        metrics.supersede_probes.increment();
        if newer.contains_key(&span.tenant, &span.trace_id, &span.span_id)? {
            return Ok(true);
        }
    }
    Ok(false)
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
    pricing: &crate::pricing::Pricing,
    filter: &SpanFilter,
    cursor: Option<&SpanCursor>,
    mask: Option<&erasure::Mask>,
) -> Result<Vec<Span>> {
    query_view_costed(
        writer,
        segments,
        metrics,
        pricing,
        filter,
        cursor,
        mask,
        &mut QueryCost::default(),
    )
}

/// [`query_view`], additionally reporting what the query had to touch.
///
/// The process-wide counters cannot answer this: several readers share them,
/// so a before/after difference attributes another thread's work to this query.
/// The cost is accumulated on the stack instead, which is free.
// Eight, because these are the store's dependencies passed explicitly rather
// than a `&Store` these free functions deliberately do not take — the write
// buffer, the segments, the metrics, and now the rate table the rollups they
// build must bind to. Bundling them into a context struct would hide which of
// them each path actually touches, which is the property this shape has.
#[allow(clippy::too_many_arguments)]
pub(crate) fn query_view_costed(
    writer: &WriteBuffer,
    segments: &[std::sync::Arc<Segment>],
    metrics: &metrics::Metrics,
    pricing: &crate::pricing::Pricing,
    filter: &SpanFilter,
    cursor: Option<&SpanCursor>,
    mask: Option<&erasure::Mask>,
    cost: &mut QueryCost,
) -> Result<Vec<Span>> {
    // A pending erasure hides its subject from every read path, and it hides
    // it HERE — beside the filter, inside the limit accounting — rather than
    // by post-filtering results, so a limited page is still a full page.
    let visible = |span: &Span| mask.map_or(true, |mask| !mask.covers(span));
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
        let mut spans = query_view_costed(
            writer, segments, metrics, pricing, &unlimited, cursor, mask, cost,
        )?;
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
    // Parsed once for the whole query. Tokenizing the needle per candidate
    // span would cost more than the index saves.
    let content = filter.content.as_deref().map(content::Query::new);
    {
        let mut result = Vec::new();

        // Limited queries take the lazy path: per-source candidates stay as
        // segment posting/record offsets and a k-way merge decodes one head per
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
                    visible(span)
                        && span_matches(span, filter, content.as_ref())
                        && cursor.map_or(true, |position| span_after_cursor(span, position))
                })
                .map(|span| Span::clone(span))
                .collect();
            sort_spans(&mut buffered);

            enum Source<'a> {
                Parsed(Vec<Span>),
                Lazy {
                    seg: &'a segment::Segment,
                    // Owned when the content index produced the candidates,
                    // borrowed when an attribute posting list did.
                    offsets: Cow<'a, [u64]>,
                    /// This segment's index in `segments`.
                    ///
                    /// Carried explicitly because it is NOT the source's own
                    /// index. Supersedence used to derive one from the other
                    /// positionally, which held only while every segment
                    /// produced a source; each `continue` in the loop below
                    /// compresses `sources` and not `segments`, so a pruned
                    /// segment shifted every later source onto the wrong
                    /// neighbour and made a segment supersede ITSELF —
                    /// dropping every row it held. Reachable through time
                    /// pruning, and reachable on the default path once
                    /// content pruning existed.
                    segment_position: usize,
                },
            }
            let mut sources: Vec<(Source<'_>, usize)> = vec![(Source::Parsed(buffered), 0)];
            for (segment_position, segment) in segments.iter().enumerate() {
                let seg = &segment.seg;
                // Skip whole segments that cannot hold a matching timestamp.
                // This is the only filter that eliminates a segment without
                // reading it, and "the last N minutes" is the commonest
                // filter an observability store sees.
                metrics.segments_examined.increment();
                cost.segments_examined += 1;
                if !seg.may_contain_timestamps(filter.since_ns, filter.until_ns) {
                    metrics.segments_pruned_by_time.increment();
                    cost.segments_pruned += 1;
                    continue;
                }
                if content
                    .as_ref()
                    .is_some_and(|query| !seg.may_contain_content(query))
                {
                    metrics.segments_pruned_by_content.increment();
                    cost.segments_pruned += 1;
                    continue;
                }
                let offsets = select_probe(seg, filter, content.as_ref(), metrics)?;
                let position = match cursor {
                    Some(cursor) => first_offset_after(seg, &offsets, cursor)?,
                    None => 0,
                };
                sources.push((
                    Source::Lazy {
                        seg,
                        offsets,
                        segment_position,
                    },
                    position,
                ));
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
                    Source::Lazy { seg, offsets, .. } => {
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
            let mut emitted: std::collections::HashSet<(String, String, String)> =
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
                if !visible(&span) {
                    continue;
                }
                if index != 0
                    && (!span_matches(&span, filter, content.as_ref())
                        || cursor.is_some_and(|position| !span_after_cursor(&span, position)))
                {
                    continue;
                }
                let key = (
                    span.tenant.clone(),
                    span.trace_id.clone(),
                    span.span_id.clone(),
                );
                if emitted.contains(&key) {
                    continue;
                }
                // Primary-key precedence: the write buffer always wins; among
                // segments, a LATER segment means a later flush and a newer
                // version, so a candidate loses to any higher-precedence
                // source that also holds its key.
                //
                // The skip is anchored to the segment's OWN index, which the
                // source carries. Deriving it from the source's index instead
                // was correct only when nothing was pruned.
                let superseded = match &sources[index].0 {
                    Source::Parsed(_) => false,
                    Source::Lazy {
                        segment_position, ..
                    } => {
                        writer.contains_key(&span.tenant, &span.trace_id, &span.span_id)
                            || superseded_by_newer(
                                segments,
                                *segment_position,
                                &span,
                                metrics,
                                pricing,
                            )?
                    }
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
            cost.segments_examined += 1;
            if !seg.may_contain_timestamps(filter.since_ns, filter.until_ns) {
                metrics.segments_pruned_by_time.increment();
                cost.segments_pruned += 1;
                continue;
            }
            if content
                .as_ref()
                .is_some_and(|query| !seg.may_contain_content(query))
            {
                metrics.segments_pruned_by_content.increment();
                cost.segments_pruned += 1;
                continue;
            }
            let offsets = select_probe(seg, filter, content.as_ref(), metrics)?;
            for offset in offsets.iter() {
                let record = seg.record_at_offset(*offset).map_err(segment_error)?;
                let span = record_to_span(&record)?;
                if !visible(&span)
                    || !span_matches(&span, filter, content.as_ref())
                    || cursor.is_some_and(|bound| !span_after_cursor(&span, bound))
                {
                    continue;
                }
                if writer.contains_key(&span.tenant, &span.trace_id, &span.span_id) {
                    continue; // the buffer holds a newer version
                }
                if !superseded_by_newer(segments, position, &span, metrics, pricing)? {
                    result.push(span);
                }
            }
        }
        for span in writer.spans.iter() {
            if visible(span)
                && span_matches(span, filter, content.as_ref())
                && cursor.map_or(true, |bound| span_after_cursor(span, bound))
            {
                result.push(Span::clone(span));
            }
        }

        sort_spans(&mut result);
        Ok(result)
    }
}

/// Calls `visit` once per matching span without ever holding them all.
///
/// This is [`query_view`]'s unlimited path with the collecting `Vec` replaced
/// by a visitor, and it exists because the aggregation routes fold a window
/// into a few hundred bytes: materializing a million spans to produce a
/// twenty-bucket histogram would make the answer's cost proportional to the
/// corpus rather than to the answer. Order is not established — no caller of a
/// commutative fold needs it, and sorting would reintroduce the full
/// materialization this avoids.
///
/// Primary-key precedence matches the query path exactly: a candidate is
/// dropped if the write buffer or any newer segment also holds its key.
// Eight, because these are the store's dependencies passed explicitly rather
// than a `&Store` these free functions deliberately do not take — the write
// buffer, the segments, the metrics, and now the rate table the rollups they
// build must bind to. Bundling them into a context struct would hide which of
// them each path actually touches, which is the property this shape has.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_view(
    writer: &WriteBuffer,
    segments: &[std::sync::Arc<Segment>],
    metrics: &metrics::Metrics,
    pricing: &crate::pricing::Pricing,
    filter: &SpanFilter,
    mask: Option<&erasure::Mask>,
    cost: &mut QueryCost,
    visit: &mut impl FnMut(&Span),
) -> Result<()> {
    // Same rule as the query path: a pending erasure's subject never reaches
    // an aggregation, even before its bytes are rewritten away.
    let visible = |span: &Span| mask.map_or(true, |mask| !mask.covers(span));
    // Parsed once for the whole fold, exactly as the query path does it: the
    // tokenizer runs per call, and running it per segment would put it on the
    // hot loop of every aggregation.
    let content = filter.content.as_deref().map(content::Query::new);
    for (position, segment) in segments.iter().enumerate() {
        let seg = &segment.seg;
        metrics.segments_examined.increment();
        cost.segments_examined += 1;
        if !seg.may_contain_timestamps(filter.since_ns, filter.until_ns) {
            metrics.segments_pruned_by_time.increment();
            cost.segments_pruned += 1;
            continue;
        }
        // A segment whose content index cannot hold every queried word is
        // skipped whole, like a time range that cannot match. Counted into
        // `segments_pruned` so an aggregation reports the same saving the
        // search path does.
        if content
            .as_ref()
            .is_some_and(|query| !seg.may_contain_content(query))
        {
            metrics.segments_pruned_by_content.increment();
            cost.segments_pruned += 1;
            continue;
        }
        let offsets = select_probe(seg, filter, content.as_ref(), metrics)?;
        for offset in offsets.iter() {
            let record = seg.record_at_offset(*offset).map_err(segment_error)?;
            let span = record_to_span(&record)?;
            if !visible(&span) || !span_matches(&span, filter, content.as_ref()) {
                continue;
            }
            if writer.contains_key(&span.tenant, &span.trace_id, &span.span_id) {
                continue; // the buffer holds a newer version
            }
            if !superseded_by_newer(segments, position, &span, metrics, pricing)? {
                visit(&span);
            }
        }
    }
    for span in writer.spans.iter() {
        if visible(span) && span_matches(span, filter, content.as_ref()) {
            visit(span);
        }
    }
    Ok(())
}

impl Store {
    /// Folds every span matching `filter` through `visit`, in constant memory.
    ///
    /// `filter.limit` and `filter.sort` are ignored: a fold is over the whole
    /// match set, and truncating or ordering it would silently change the
    /// aggregate rather than the presentation.
    ///
    /// **The engine locks are not held across the scan.** A fold reads the
    /// whole corpus, so holding the writer lock for its duration would let any
    /// aggregation — a dashboard drawing four of them at once — stall ingest
    /// for as long as the scan takes. The work happens against a
    /// [`SnapshotView`], whose cost is one copy of the bounded write buffer
    /// and one `Arc` clone per segment. That also makes the answer coherent:
    /// a fold sees one instant of the store rather than a moving one.
    pub fn fold_spans(
        &self,
        filter: &SpanFilter,
        mut visit: impl FnMut(&Span),
    ) -> Result<QueryCost> {
        let started = std::time::Instant::now();
        let mut cost = QueryCost::default();
        if let Some(session_id) = &filter.session {
            // A session unions several attribute keys, which no single index
            // expresses. Its span count is bounded by one conversation, so
            // resolving it up front costs a conversation, not a corpus.
            let content = filter.content.as_deref().map(content::Query::new);
            for span in self.resolve_session_spans(filter.tenant.as_deref(), session_id)? {
                if span_matches(&span, filter, content.as_ref()) {
                    visit(&span);
                }
            }
        } else {
            let view = self.snapshot()?; // takes both locks, then releases them
            view.fold(filter, &mut cost, &mut visit)?;
        }
        cost.elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        Ok(cost)
    }

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
            buffer_age_seconds: writer.oldest_at.map(|oldest| oldest.elapsed().as_secs()),
        })
    }

    /// Removes spans older than the configured TTL and returns the number
    /// removed. A zero TTL disables expiration.
    /// Records one annotation durably (see [`annotations::Annotation`]).
    ///
    /// An annotation addressed to a subject a PENDING erasure covers is
    /// acknowledged and deliberately not stored — the same admission barrier
    /// spans get, for the same reason: without it, an annotation landing
    /// between the erasure's annotation drop and its settle would attach
    /// judgment to data that no longer exists, invisibly until the mask
    /// lifted. After the erasure settles, the same address is annotatable
    /// again (there is nothing there to annotate until new data arrives, but
    /// the barrier is the erasure's, not a permanent ban).
    pub fn annotate(&self, annotation: annotations::Annotation) -> Result<()> {
        // Check and append under one read acquisition of the erasure gate: a
        // one-shot check raced `begin`, and an annotation slipping in
        // between the erasure's annotation drop and its settle would attach
        // judgment to erased data. Under the gate the append either
        // completes before `begin` (and the erasure's own drop sweeps it) or
        // starts after (and the mask refuses it).
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        if let Some(mask) = self.erasure_mask() {
            if mask.covers_annotation(&annotation) {
                return Ok(());
            }
        }
        // A score's address must hold at write time: its experiment exists,
        // belongs to the score's tenant, and lists the example. Validated
        // under the eval log's own mutex, and under the same gate hold as
        // the append — a tombstone or erasure cannot slide between check
        // and write.
        if let Some(experiment_id) = annotation.experiment_id {
            annotation.validate_subject()?;
            self.evals
                .validate_score(&annotation.tenant, experiment_id, &annotation.example_id)?;
        }
        self.annotations.append(annotation)
    }

    /// Annotations for a trace, optionally narrowed to one span or name.
    pub fn annotations(
        &self,
        trace_id: &str,
        span_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Vec<annotations::Annotation>> {
        self.annotations_in(None, trace_id, span_id, name)
    }

    /// [`Self::annotations`] under a tenant scope; `None` is the operator
    /// view, `Some(t)` only tenant `t`'s judgments.
    pub fn annotations_in(
        &self,
        tenant: Option<&str>,
        trace_id: &str,
        span_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Vec<annotations::Annotation>> {
        let mut found = self.annotations.query(tenant, trace_id, span_id, name)?;
        // "Invisible to every query" includes the judgments ABOUT the
        // subject: an annotation addressed to a pending erasure's span is
        // withheld exactly as the span is, and the purge drops it before the
        // mask lifts.
        if let Some(mask) = self.erasure_mask() {
            found.retain(|annotation| !mask.covers_annotation(annotation));
        }
        Ok(found)
    }

    /// Annotations matching `narrow`, newest first, across every trace unless
    /// one is named. See [`annotations::AnnotationQuery`].
    pub fn search_annotations(
        &self,
        narrow: &annotations::AnnotationQuery<'_>,
    ) -> Result<Vec<annotations::Annotation>> {
        let mut found = self.annotations.search(narrow)?;
        // Same rule as [`Self::annotations`], for the same reason.
        if let Some(mask) = self.erasure_mask() {
            found.retain(|annotation| !mask.covers_annotation(annotation));
        }
        Ok(found)
    }

    // ------------------------------------------------------------- evals
    //
    // Every eval MUTATION holds the erasure gate's read half across its
    // whole validate+append, exactly as span and annotation admission do: a
    // tenant erasure's `begin` takes the write half, so a dataset, version,
    // experiment, run or tombstone append is wholly before the erasure
    // (the barrier rewrite removes it) or wholly after (the mask refuses it
    // with a 409-shaped Conflict) — never astride the transition.

    /// Creates a dataset owned by `tenant` (empty = the default tenant) and
    /// returns its id.
    pub fn create_dataset(&self, tenant: &str, name: &str) -> Result<u64> {
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        self.evals
            .create_dataset(mask.as_deref(), tenant, name, unix_now_ns())
    }

    /// Creates (or idempotently re-finds) a dataset version. `scope` is a
    /// bound principal's tenant; a dataset outside the scope reads as
    /// nonexistent.
    pub fn create_dataset_version(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
        parent: Option<String>,
        provenance: Option<Value>,
        examples: Vec<evals::NewExample>,
    ) -> Result<evals::VersionOutcome> {
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        // The payload interlock: touch first, then check existence — the
        // registry insert is what makes a concurrent TTL sweep's unlink
        // yield, exactly as `store_payload` does for fresh writes. An
        // example whose reference is already gone is refused, not recorded
        // dangling.
        let verify = |reference: &str| -> Result<bool> {
            if let Ok(mut touched) = self.recent_payloads.lock() {
                touched.insert(reference.to_owned(), Instant::now());
            }
            let Some(hash) = reference.strip_prefix("sha256/") else {
                return Ok(false);
            };
            Ok(payload::payload_path(&self.directory, hash).exists())
        };
        // For a BOUND caller, precompute which of the bodies' references the
        // tenant's own SPANS hold (the eval-side half is checked inside the
        // log's mutex, where its state is consistent). Computed out here
        // because it reads the buffer and pinned segments, and the eval
        // mutex is a leaf that must not reach into engine locks.
        let spans_hold = match scope {
            None => None,
            Some(tenant) => {
                let mut wanted: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for example in &examples {
                    for value in [Some(&example.input), example.expected.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        evals::collect_payload_refs_from(value, &mut wanted);
                    }
                }
                let mut held: std::collections::HashSet<String> = std::collections::HashSet::new();
                for reference in wanted {
                    if self.tenant_spans_hold_reference(tenant, &reference)? {
                        held.insert(reference);
                    }
                }
                Some(held)
            }
        };
        self.evals.create_version(
            mask.as_deref(),
            scope,
            dataset_id,
            parent,
            provenance,
            examples,
            unix_now_ns(),
            &verify,
            spans_hold.as_ref(),
        )
    }

    /// Creates an experiment against a dataset version; the experiment
    /// inherits the dataset's tenant.
    pub fn create_experiment(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
        dataset_version: &str,
        name: &str,
        config: Option<Value>,
    ) -> Result<u64> {
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        self.evals.create_experiment(
            mask.as_deref(),
            scope,
            dataset_id,
            dataset_version,
            name,
            config,
            unix_now_ns(),
        )
    }

    /// Records one task run for an experiment's example — the
    /// experiment→trace link; execution itself stays external.
    pub fn record_eval_run(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
        example_id: &str,
        trace_id: &str,
        span_id: &str,
    ) -> Result<()> {
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        self.evals.record_run(
            mask.as_deref(),
            scope,
            experiment_id,
            example_id,
            trace_id,
            span_id,
            unix_now_ns(),
        )
    }

    /// Tombstones a dataset version — logical deletion with defined effects
    /// (see [`evals::VersionTombstone`]). Idempotent.
    pub fn tombstone_dataset_version(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
        version_id: &str,
        reason: &str,
    ) -> Result<bool> {
        let _gate = self
            .erasure_gate
            .read()
            .map_err(|_| Error::LockPoisoned("erasure gate"))?;
        let mask = self.erasure_mask();
        self.evals.tombstone_version(
            mask.as_deref(),
            scope,
            dataset_id,
            version_id,
            reason,
            unix_now_ns(),
        )
    }

    /// Whether a pending tenant erasure hides this tenant's eval entities
    /// from reads — the read half of the eval barrier.
    fn eval_hidden(&self, tenant: &str) -> bool {
        self.erasure_mask()
            .is_some_and(|mask| mask.covers_tenant(tenant))
    }

    /// Datasets visible under `scope`, versions summarized.
    pub fn datasets(&self, scope: Option<&str>) -> Result<Vec<evals::DatasetView>> {
        let mut views = self.evals.datasets(scope)?;
        views.retain(|view| !self.eval_hidden(&view.dataset.tenant));
        Ok(views)
    }

    /// One dataset, or `None` (unknown, foreign to the scope, or hidden by
    /// a pending tenant erasure).
    pub fn dataset(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
    ) -> Result<Option<evals::DatasetView>> {
        Ok(self
            .evals
            .dataset(scope, dataset_id)?
            .filter(|view| !self.eval_hidden(&view.dataset.tenant)))
    }

    /// One dataset version with bodies, the tombstone hiding it, or `None`.
    #[allow(clippy::type_complexity)]
    pub fn dataset_version(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
        version_id: &str,
    ) -> Result<Option<std::result::Result<evals::VersionView, evals::VersionTombstone>>> {
        Ok(self
            .evals
            .version(scope, dataset_id, version_id)?
            .filter(|found| {
                let tenant = match found {
                    Ok(view) => &view.version.tenant,
                    Err(tombstone) => &tombstone.tenant,
                };
                !self.eval_hidden(tenant)
            }))
    }

    /// Experiments visible under `scope`, optionally narrowed to a dataset.
    pub fn experiments(
        &self,
        scope: Option<&str>,
        dataset_id: Option<u64>,
    ) -> Result<Vec<evals::ExperimentView>> {
        let mut views = self.evals.experiments(scope, dataset_id)?;
        views.retain(|view| !self.eval_hidden(&view.experiment.tenant));
        Ok(views)
    }

    /// One experiment, or `None`.
    pub fn experiment(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
    ) -> Result<Option<evals::ExperimentView>> {
        Ok(self
            .evals
            .experiment(scope, experiment_id)?
            .filter(|view| !self.eval_hidden(&view.experiment.tenant)))
    }

    /// An experiment's recorded runs, or `None`.
    pub fn eval_runs(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
    ) -> Result<Option<Vec<evals::Run>>> {
        if self.experiment(scope, experiment_id)?.is_none() {
            return Ok(None);
        }
        self.evals.runs(scope, experiment_id)
    }

    /// An experiment's scores — the annotations addressed to it — newest
    /// first, or `None` when the experiment is unknown to the scope.
    pub fn experiment_scores(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
        limit: Option<usize>,
    ) -> Result<Option<Vec<annotations::Annotation>>> {
        if self.experiment(scope, experiment_id)?.is_none() {
            return Ok(None);
        }
        let narrow = annotations::AnnotationQuery {
            experiment_id: Some(experiment_id),
            limit,
            ..annotations::AnnotationQuery::default()
        };
        Ok(Some(self.search_annotations(&narrow)?))
    }

    /// Score distributions for one experiment, or `None`. Scores are
    /// deduplicated per `(example, name)` by latest timestamp before any
    /// statistic — a retried scorer moves a number, it never double-counts.
    pub fn experiment_summary(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
    ) -> Result<Option<evals::ExperimentSummary>> {
        if self.experiment(scope, experiment_id)?.is_none() {
            return Ok(None);
        }
        let examples = self
            .evals
            .experiment_examples(scope, experiment_id)?
            .unwrap_or_default();
        let scores = self
            .experiment_scores(scope, experiment_id, None)?
            .unwrap_or_default();
        Ok(Some(evals::summarize_scores(
            experiment_id,
            examples.len(),
            &scores,
        )))
    }

    /// Experiment-over-experiment comparison, joined on `(example, name)`
    /// after the same dedup as [`Self::experiment_summary`]. Numeric scores
    /// compare higher-is-better; booleans as 0/1 — a convention, stated,
    /// not configurable in this milestone. `None` when either experiment is
    /// unknown to the scope.
    pub fn experiment_diff(
        &self,
        scope: Option<&str>,
        base: u64,
        candidate: u64,
    ) -> Result<Option<evals::ExperimentDiff>> {
        let (Some(base_scores), Some(candidate_scores)) = (
            self.experiment_scores(scope, base, None)?,
            self.experiment_scores(scope, candidate, None)?,
        ) else {
            return Ok(None);
        };
        Ok(Some(evals::diff_scores(
            base,
            candidate,
            &base_scores,
            &candidate_scores,
        )))
    }

    /// Per-tenant usage accounting: one exact fold over a pinned snapshot.
    ///
    /// This is the ACCOUNTING surface tenancy promises — enforcement is a
    /// later milestone's problem — and it is O(store) by construction, so it
    /// is an on-demand answer, not something to poll per minute against a
    /// hundred-million-span corpus. `scope` narrows a bound principal to its
    /// own row.
    pub fn tenant_usage(&self, scope: Option<&str>) -> Result<Vec<TenantUsage>> {
        struct Accumulator {
            spans: u64,
            traces: std::collections::HashSet<String>,
            bytes_approx: u64,
            payload_refs: std::collections::HashMap<String, u64>,
            first_start_ns: u64,
            last_end_ns: u64,
        }
        let mut by_tenant: std::collections::HashMap<String, Accumulator> =
            std::collections::HashMap::new();
        let filter = SpanFilter {
            tenant: scope.map(str::to_owned),
            ..SpanFilter::default()
        };
        self.fold_spans(&filter, |span| {
            let entry = by_tenant
                .entry(span.tenant.clone())
                .or_insert_with(|| Accumulator {
                    spans: 0,
                    traces: std::collections::HashSet::new(),
                    bytes_approx: 0,
                    payload_refs: std::collections::HashMap::new(),
                    first_start_ns: u64::MAX,
                    last_end_ns: 0,
                });
            entry.spans += 1;
            entry.traces.insert(span.trace_id.clone());
            entry.bytes_approx += serde_json::to_vec(span).map_or(0, |bytes| bytes.len() as u64);
            let mut refs = std::collections::HashSet::new();
            erasure::payload_refs_of(span, &mut refs);
            for reference in refs {
                let bytes = payload_ref_bytes(span, &reference);
                entry.payload_refs.entry(reference).or_insert(bytes);
            }
            entry.first_start_ns = entry.first_start_ns.min(span.start_time_ns);
            entry.last_end_ns = entry.last_end_ns.max(span.end_time_ns);
        })?;
        let mut rows: Vec<TenantUsage> = by_tenant
            .into_iter()
            .map(|(tenant, accumulated)| TenantUsage {
                tenant,
                spans: accumulated.spans,
                traces: accumulated.traces.len() as u64,
                bytes_approx: accumulated.bytes_approx,
                payload_bytes_approx: accumulated.payload_refs.values().sum(),
                first_start_ns: match accumulated.first_start_ns {
                    u64::MAX => 0,
                    other => other,
                },
                last_end_ns: accumulated.last_end_ns,
            })
            .collect();
        rows.sort_by(|left, right| left.tenant.cmp(&right.tenant));
        Ok(rows)
    }

    /// Reads an offloaded payload by its `sha256/<hex>` reference.
    pub fn payload(&self, reference: &str) -> Result<Option<Vec<u8>>> {
        self.payload_in(None, reference)
    }

    /// [`Self::payload`] under a tenant scope.
    ///
    /// Content addressing is store-global, and for the OPERATOR the full
    /// hash is the capability — knowing it means having read a span that
    /// disclosed it. Across a tenant boundary that argument fails: the hash
    /// of GUESSABLE content is computable, and a 200 would confirm another
    /// tenant stored those exact bytes. So a bound scope must also prove
    /// REACHABILITY — some span or dataset example of that tenant carries
    /// the reference — and an unreachable payload answers `None`, exactly
    /// as an absent one does. The cost is a prefiltered probe (sidecar
    /// reference sets, then the tenant posting), paid only by bound
    /// callers on a fetch-shaped request.
    pub fn payload_in(&self, tenant: Option<&str>, reference: &str) -> Result<Option<Vec<u8>>> {
        // A payload a pending erasure is due to account for is withheld
        // while it is pending — the bytes may be seconds from deletion, and
        // serving them from under an erasure in progress is the same failure
        // as serving the span. A shared reference that survives (retained
        // for live spans outside the subject) resurfaces at settle.
        if let Some(mask) = self.erasure_mask() {
            if mask.covers_payload_file(reference) {
                return Ok(None);
            }
            // A whole-tenant erasure discovers its span-held references only
            // as its purge walks them, so `payload_files` is still filling
            // while the mask already hides the tenant. A scoped fetch must
            // not race that gap: the tenant is being erased, so nothing of
            // its is served, whether or not this exact reference has been
            // enumerated yet. The operator (unscoped) fetch is governed by
            // `payload_files` alone, as it was — its capability is the hash.
            if let Some(tenant) = tenant {
                if mask.covers_tenant(tenant) {
                    return Ok(None);
                }
            }
        }
        if let Some(tenant) = tenant {
            if !self.tenant_holds_reference(tenant, reference)? {
                return Ok(None);
            }
        }
        payload::load_payload(&self.directory, reference)
    }

    /// The eval log's payload references, for the liveness union in
    /// [`Self::live_payload_refs`] (which lives in the analytics module and
    /// reaches the eval log through this).
    pub(crate) fn eval_payload_refs(&self) -> Result<std::collections::HashSet<String>> {
        self.evals.payload_refs()
    }

    /// Every `$payload` reference carried by any of `tenant`'s spans, read
    /// MASK-FREE from the raw buffer and segments — a query read would apply
    /// the pending tenant mask and see none of them. Used to record a tenant
    /// erasure's span-held refs durably before its purge removes the spans.
    fn tenant_span_payload_refs(&self, tenant: &str) -> Result<std::collections::HashSet<String>> {
        let mut refs = std::collections::HashSet::new();
        {
            let writer = self.lock_writer()?;
            for span in writer.spans.iter() {
                if span.tenant == tenant {
                    erasure::payload_refs_of(span, &mut refs);
                }
            }
        }
        for segment in self.pin_segments()? {
            for span in segment.spans_parsed()? {
                if span.tenant == tenant {
                    erasure::payload_refs_of(&span, &mut refs);
                }
            }
        }
        Ok(refs)
    }

    /// Whether any of `tenant`'s spans or dataset examples carries
    /// `reference` — the reachability proof behind [`Self::payload_in`].
    fn tenant_holds_reference(&self, tenant: &str, reference: &str) -> Result<bool> {
        if self.tenant_spans_hold_reference(tenant, reference)? {
            return Ok(true);
        }
        // The tenant's dataset examples: an example legitimately outlives
        // its source span, and its holder must keep fetch access.
        self.evals.tenant_references(tenant, reference)
    }

    /// The span-side half of [`Self::tenant_holds_reference`]: buffer and
    /// segments only, no eval-log locks — callable while building inputs
    /// for an eval mutation that will hold the eval mutex itself.
    fn tenant_spans_hold_reference(&self, tenant: &str, reference: &str) -> Result<bool> {
        // The write buffer first: cheap, and where the freshest refs live.
        {
            let writer = self.lock_writer()?;
            let mut refs = std::collections::HashSet::new();
            for span in writer.spans.iter() {
                if span.tenant == tenant {
                    erasure::payload_refs_of(span, &mut refs);
                    if refs.contains(reference) {
                        return Ok(true);
                    }
                }
            }
        }
        // Segments, doubly prefiltered: only segments whose sidecar holds
        // the reference at all, and within them only the tenant's records.
        for segment in self.pin_segments()? {
            let may_hold = match rollup_file::load(
                &segment.path,
                segment.rollup_binding(self.pricing_fingerprint()),
            ) {
                Some(rollup) => rollup.payload_refs.contains(reference),
                None => true, // no usable sidecar cannot be ruled out
            };
            if !may_hold {
                continue;
            }
            // The default (empty) tenant carries no posting — it is never
            // indexed, so single-tenant stores stay byte-identical — so the
            // tenant posting cannot answer for it. Scan every record and let
            // the decoded tenant decide, exactly as `select_probe` falls
            // through to `record_offsets` for an explicit `Some("")` scope.
            // A non-empty tenant keeps its narrow posting probe.
            let offsets: Vec<u64> = if tenant.is_empty() {
                segment.seg.record_offsets().to_vec()
            } else {
                segment
                    .seg
                    .attribute_candidate_offsets(IDX_TENANT, tenant)
                    .to_vec()
            };
            for offset in offsets {
                let record = segment
                    .seg
                    .record_at_offset(offset)
                    .map_err(segment_error)?;
                let span = record_to_span(&record)?;
                if span.tenant != tenant {
                    continue;
                }
                let mut refs = std::collections::HashSet::new();
                erasure::payload_refs_of(&span, &mut refs);
                if refs.contains(reference) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Expires spans, annotations, and payload files older than their
    /// tenant's retention window (no-op when no TTL is configured at all).
    /// Every tenant expires on its own cutoff: its override if listed in
    /// [`Config::tenant_ttl_seconds`], else the global TTL, else never.
    pub fn compact_expired(&self) -> Result<usize> {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let Some(cutoffs) = ExpiryCutoffs::from_config(&self.config, now_ns) else {
            return Ok(0);
        };
        let _maintenance = self.lock_maintenance()?;
        let removed = self.expire_by_policy_locked(&cutoffs)?;
        // The satellite stores age on the same per-tenant windows:
        // annotations by their own timestamps and their own tenant (scores
        // are exempt — eval retention, not trace retention), payload files
        // by mtime against the EARLIEST configured cutoff — the mtime gate
        // only protects brand-new files, liveness and touch immunity carry
        // the real safety, so the conservative bound costs at most a
        // lingering orphan.
        self.annotations
            .drop_older_than(&|tenant| cutoffs.cutoff_for(tenant))?;
        let cutoff_time = UNIX_EPOCH + std::time::Duration::from_nanos(cutoffs.earliest());
        // Nothing to protect means nothing to compute.
        //
        // `live_payload_refs` walks every segment, and it was run on every
        // tick even for a store that has never offloaded a single value —
        // `sweep_expired` would then find no payload directory and return
        // immediately, so the whole walk was in service of an empty answer.
        // The directory's existence is the exact test: no directory, no files
        // to sweep, no reference set needed. It is checked here rather than
        // only inside the sweep because the cost being avoided is the
        // caller's, not the sweep's.
        //
        // Deliberately NOT gated on `config.payload_threshold`: a store whose
        // threshold was removed still holds the files written while it was
        // set, and those must go on aging out.
        // Unconditionally: the touch registry's only pruner used to live
        // inside the sweep, so skipping the sweep would have turned "this
        // store has no payloads" into "this store's touch registry grows
        // without bound".
        payload::prune_touch_registry(&self.recent_payloads)?;
        if self.directory.join(payload::PAYLOAD_DIR).exists() {
            // Live references computed AFTER span expiry, so payloads
            // referenced only by just-expired spans become sweepable.
            let live_refs = self.live_payload_refs()?;
            payload::sweep_expired(
                &self.directory,
                cutoff_time,
                &live_refs,
                &self.recent_payloads,
            )?;
        }
        // The deletion is already durable in every domain it touched — that is
        // invariant 10's discipline and it has not moved. What a generation
        // adds is publication: the next checkpoint stops naming the expired
        // bytes. That checkpoint is deliberately NOT taken here. It seals the
        // write buffer, and a primitive that expires must not also decide when
        // to seal; the maintenance cadence and `pin_generation` both publish
        // on their own, so the deletion is in a published generation within
        // one maintenance interval, or immediately if a backup asks.
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
    /// **A run merges into a GROUP of segments, not one.** Capping the output
    /// by truncating the run instead left a cap-sized segment at the tail,
    /// one tier above the segments behind it, and those were then unreachable
    /// forever — tail-only means nothing behind the tail is ever a candidate
    /// again. So the run is split into consecutive groups of at most
    /// [`CompactionConfig::max_segment_bytes`] and each becomes one output,
    /// taking ids in group order ([`merge_chunks`]). Last-write-wins survives
    /// because that order is the inputs' order: a key in two groups lands in
    /// two outputs, and the later output holds the later version.
    ///
    /// **Crash safety** reuses the existing supersede journal, one marker per
    /// input, each naming the whole output group. Recovery deletes an input
    /// only once every output is present and parses; if any is missing it
    /// deletes the ones that landed and the inputs stay authoritative, so the
    /// merge is simply retried. Outputs are written and renamed into place
    /// BEFORE any input is deleted, so no window drops data.
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
        // loop until nothing qualifies. Bounded by the tier count. Choosing
        // the run is `merge_tail_run`'s own job, done under the locks it pins
        // with — scanning out here first only created a window for a seal to
        // land and make the answer stale.
        // A tick compacts the backlog it FOUND. Every segment sealed after
        // this point is left for the next one: merging those too would keep
        // this call running for as long as writes keep arriving, and a
        // maintenance pass that never returns is not maintenance. Measured
        // without it, one call merged away 2,213 segments and was still going.
        let watermark = self.next_segment.load(Ordering::Relaxed);
        loop {
            let merged = self.merge_tail_run(&settings, watermark)?;
            if merged == 0 {
                break;
            }
            merged_away += merged;
        }
        Ok(merged_away)
    }

    /// Applies the buffer's non-volume bounds: seals on age, and answers
    /// observed key shadowing with a seal plus a bounded deduplicating merge.
    ///
    /// The engine implements the policy; the caller owns the clock, exactly
    /// as with TTL. `traza-server` calls this from its maintenance tick;
    /// an embedding process that wants the bounds enforced while idle should
    /// call it on a similar cadence (every few seconds is plenty — both
    /// checks are a lock acquisition and a comparison when nothing is due).
    ///
    /// Age also gates on the ingest path, so an actively-written store seals
    /// on its next batch; this call exists for the store that goes quiet with
    /// spans still buffered — the case the volume thresholds structurally
    /// cannot bound, measured in production at 36 days of buffered upserts
    /// that disqualified every segment's rollup for the whole of it.
    pub fn maintain_buffer(&self) -> Result<()> {
        let age_due = {
            let writer = self.lock_writer()?;
            match (self.config.max_buffer_age, writer.oldest_at) {
                (Some(limit), Some(oldest)) => oldest.elapsed() >= limit,
                _ => false,
            }
        };
        // SkipIfBusy: a seal already in flight covers everything buffered.
        // The counter records seals that actually published, not attempts —
        // an expiry pass holding the permit for minutes must not turn every
        // tick into a phantom age seal.
        if age_due && self.seal(SealWait::SkipIfBusy)? {
            self.metrics.segment_seals_age.increment();
        }

        // The shadow pass merges; it never seals. Sealing here answered
        // buffer-caused shadowing, but a buffer key a client is still
        // updating re-shadows the moment it is sealed, so the pass either
        // churned a rewrite per interval or manufactured unmergeable crumb
        // segments when it could not merge at all (compaction off, or the
        // collision out of budget). Buffer-caused shadowing is the age
        // bound's problem — bounded, transient — and only segment-versus-
        // segment shadowing, which a merge genuinely retires, latches
        // (see `fold_analytics`). Without compaction there is nothing this
        // pass could do, so it does not run.
        if self.config.shadow_seal && self.config.compaction.is_some() && self.take_shadow_pass()? {
            let merged = self.compact_shadowed()?;
            if merged > 0 {
                self.metrics.shadow_merges.increment();
            }
            self.finish_shadow_pass(merged > 0)?;
        }
        Ok(())
    }

    /// Consumes the shadowing latch if the pass clock allows. The clock is
    /// advanced only by [`Self::finish_shadow_pass`], so a quiet store pays a
    /// single atomic load per call.
    fn take_shadow_pass(&self) -> Result<bool> {
        if !self
            .shadowing_observed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(false);
        }
        let pass = self
            .shadow_pass
            .lock()
            .map_err(|_| Error::LockPoisoned("shadow_pass"))?;
        if pass
            .not_before
            .is_some_and(|not_before| Instant::now() < not_before)
        {
            return Ok(false);
        }
        drop(pass);
        Ok(self
            .shadowing_observed
            .swap(false, std::sync::atomic::Ordering::Relaxed))
    }

    /// Schedules the next shadow pass. Success cools down for a fixed
    /// interval — a workload that re-poisons after every merge gets a bounded
    /// rewrite rate, not a rewrite per observation. Futility backs off
    /// exponentially: a store whose collision lies beyond the byte budget
    /// (or whose scan is blocked by a sidecar-less segment) re-latches after
    /// every aggregation forever, and re-scanning it every interval would buy
    /// sidecar reads with no merge at the end.
    fn finish_shadow_pass(&self, merged: bool) -> Result<()> {
        let mut pass = self
            .shadow_pass
            .lock()
            .map_err(|_| Error::LockPoisoned("shadow_pass"))?;
        if merged {
            pass.backoff = SHADOW_PASS_MIN_INTERVAL;
            pass.not_before = Some(Instant::now() + SHADOW_MERGE_COOLDOWN);
        } else {
            pass.backoff = pass.backoff.saturating_mul(2).min(SHADOW_BACKOFF_CEILING);
            pass.not_before = Some(Instant::now() + pass.backoff);
        }
        Ok(())
    }

    /// Records that an aggregation had to decode instead of using a rollup
    /// because a segment's keys are shadowed BY A NEWER SEGMENT — the state a
    /// merge retires. Called by the analytics fold; buffer-caused shadowing
    /// deliberately does not latch (see `maintain_buffer`).
    pub(crate) fn note_shadowing(&self) {
        self.shadowing_observed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Merges the shadowed tail run, if the scan finds one within the size
    /// cap, restoring every merged segment's rollup eligibility in one
    /// journaled operation. Returns segments merged away.
    ///
    /// The scan runs lock-free against a pinned segment list and reads only
    /// rollups that already exist in cache or as sidecars — it never decodes
    /// a segment — so the answer can be stale by the time the merge pins the
    /// live list. The selector therefore revalidates by path and declines
    /// rather than merging a run it did not scan. The byte budget is the
    /// compaction size cap: a run that cannot merge into one bounded output
    /// is left to tiered compaction, which owns write amplification policy.
    fn compact_shadowed(&self) -> Result<usize> {
        let Some(settings) = self.config.compaction else {
            return Ok(0);
        };
        // Zero means uncapped everywhere `max_segment_bytes` is honored
        // (see `merge_chunks`), so it must not read as a zero budget here.
        let budget = match settings.max_segment_bytes {
            0 => u64::MAX,
            bytes => bytes,
        };
        let Some(chosen) = self.shadowed_tail_suffix(budget)? else {
            return Ok(0);
        };
        if chosen.len() < 2 {
            return Ok(0);
        }
        let _maintenance = self.lock_maintenance()?;
        let watermark = self.next_segment.load(Ordering::Relaxed);
        self.merge_run(&settings, watermark, &|segments| {
            if segments.len() < chosen.len() {
                return None;
            }
            let start = segments.len() - chosen.len();
            segments[start..]
                .iter()
                .map(|segment| &segment.path)
                .eq(chosen.iter())
                .then_some(chosen.len())
        })
    }

    /// Merges the tail run, if one qualifies, into as few segments as the size
    /// cap allows. Returns segments removed, or zero if nothing qualified or
    /// the run stopped qualifying before the result could be published.
    fn merge_tail_run(&self, settings: &CompactionConfig, watermark: u64) -> Result<usize> {
        self.merge_run(settings, watermark, &|segments| {
            tail_run_to_merge(segments, settings)
        })
    }

    /// Merges the tail run `select` chooses, sharing every invariant of tiered
    /// compaction — the seal-permit pin, contiguous-block id claiming, the
    /// journal, and rollup handover. `select` sees the live segment list under
    /// both guards and must answer in microseconds: anything slower belongs
    /// before the call, with the answer revalidated here (see
    /// [`Store::compact_shadowed`]).
    fn merge_run(
        &self,
        settings: &CompactionConfig,
        watermark: u64,
        select: &dyn Fn(&[std::sync::Arc<Segment>]) -> Option<usize>,
    ) -> Result<usize> {
        let merging = Instant::now();
        // ---- pin: short critical section -------------------------------
        let (inputs, chunks, first_id, run) = {
            // A seal that has already claimed a LOWER id but not yet published
            // it would end up sorting before this merge's outputs, which hold
            // strictly older data — last-write-wins, inverted. The seal permit
            // is what rules that out: a seal holds it from drain to publish,
            // so while it is held here no seal is outstanding, and every seal
            // afterwards claims an id above the ones taken below.
            //
            // **Held only long enough to choose the run and claim the ids** —
            // microseconds — and never across the merge itself. Acquiring it
            // is the slow half, not holding it: a seal owns the permit from
            // its drain through the write, the fsync, the rename, the reopen
            // and the reconcile, so a merge arriving mid-seal waits out all
            // of that. The wait is bounded by one seal and falls on the
            // maintenance thread, which has nothing else to do.
            //
            // Most ingest is unaffected, because a seal that cannot take the
            // permit coalesces into the next one instead of waiting. Two
            // paths do wait: [`Durability::Flushed`], which must seal before
            // it acknowledges, and any mode once the buffer has reached four
            // times `flush_spans`, where waiting IS the backpressure. Both
            // wait only on the microseconds this holds it — never on a merge.
            //
            // Declining instead — the earlier design — starved compaction
            // outright. A seal is in flight for much of the time under a
            // sustained load, so a tick that checked once and gave up almost
            // never found the store quiet: measured at 25,000 spans/s, one
            // tick in sixteen achieved anything and the segment count climbed
            // without bound, which is precisely what compaction exists to
            // stop. Waiting out a seal costs a tick one seal; declining cost
            // it the whole tick, and made progress a function of the write
            // rate rather than of the backlog.
            let _permit = self
                .sealing
                .lock()
                .map_err(|_| Error::LockPoisoned("sealing"))?;
            let segments = self.lock_segments()?;
            let Some(run) = select(&segments) else {
                return Ok(0);
            };
            let start = segments.len() - run;
            let inputs: Vec<std::sync::Arc<Segment>> = segments[start..].to_vec();
            // Nothing here predates the tick, so this run is entirely work
            // that arrived while it ran. Leave it to the next one.
            if segment_number(&inputs[0].path).is_some_and(|id| id >= watermark) {
                return Ok(0);
            }
            let chunks = merge_chunks(&inputs, settings);
            // The ids are claimed HERE, under both guards, which is what keeps
            // a concurrent seal ordered correctly: every segment that appears
            // while this merge runs claims a HIGHER id, and therefore sorts
            // after the merged outputs — exactly right, it was written later.
            // Claiming the ids after the merge would invert that and let
            // merged (older) versions win over freshly sealed ones. One
            // contiguous block, so the outputs keep their groups' order among
            // themselves too.
            let first_id = self
                .next_segment
                .fetch_add(chunks.len() as u64, Ordering::Relaxed);
            (inputs, chunks, first_id, run)
        };
        let input_paths: Vec<PathBuf> = inputs.iter().map(|segment| segment.path.clone()).collect();
        let new_names: Vec<String> = (0..chunks.len() as u64)
            .map(|offset| {
                let id = first_id + offset;
                format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}")
            })
            .collect();

        // Journal the whole merge before any replacement exists, so recovery
        // can finish it from either side without inspecting content. ONE
        // journal naming every input and every output, because the merge is
        // one transaction: which way an interrupted merge has to be finished
        // is a fact about the group, and a journal that saw a single input
        // could not tell "nothing was deleted yet" from "deletion had
        // started", which are opposite answers.
        let old_names: Vec<String> = input_paths
            .iter()
            .map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let journal = write_merge_journal(&self.directory, &old_names, &new_names)?;

        // ---- merge: no engine lock held --------------------------------
        // One group at a time, so a merge holds one output's worth of spans
        // rather than the whole run's.
        let mut outputs: Vec<Sealed> = Vec::with_capacity(chunks.len());
        let written = (|| -> Result<()> {
            let mut consumed = 0usize;
            for (offset, size) in chunks.iter().enumerate() {
                let group = &inputs[consumed..consumed + size];
                consumed += size;
                // Oldest first, so a later segment's version of a key
                // overwrites an earlier one — the same last-write-wins rule
                // reads apply. Across groups the outputs' ids carry that
                // order instead.
                let mut latest: std::collections::HashMap<(String, String, String), Span> =
                    std::collections::HashMap::new();
                let mut order: Vec<(String, String, String)> = Vec::new();
                for segment in group {
                    for span in segment.spans_parsed()? {
                        let key = (
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        );
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
                outputs.push(self.write_segment(first_id + offset as u64, &merged)?);
            }
            Ok(())
        })();

        // ---- publish: short critical section, revalidated ---------------
        let published = written.is_ok() && {
            let mut segments = self.lock_segments()?;
            match run_position(&segments, &inputs) {
                Some(start) => {
                    let mut installed: Vec<(PathBuf, CachedRollup)> =
                        Vec::with_capacity(outputs.len());
                    let replacements = std::mem::take(&mut outputs)
                        .into_iter()
                        .map(|sealed| {
                            installed.push((
                                sealed.segment.path.clone(),
                                (
                                    sealed.segment.rollup_binding(self.pricing_fingerprint()),
                                    sealed.rollup,
                                ),
                            ));
                            std::sync::Arc::new(sealed.segment)
                        })
                        .collect::<Vec<_>>();
                    segments.splice(start..start + run, replacements);
                    segments.sort_by(|left, right| left.path.cmp(&right.path));
                    // Under the SAME guard that published them. The outputs'
                    // rollups are already built — the merge had to build them
                    // to write their sidecars — so handing them over here is
                    // what keeps a merge from being a latency event for the
                    // next aggregation: without it, publishing a merge leaves
                    // the next query to re-establish the replacement from
                    // disk. Measured on a one-million-span store, the worst
                    // query during compaction was 2.6 s when that rebuild was
                    // a full decode.
                    //
                    // The inputs' rollups are deliberately LEFT IN PLACE, and
                    // that is not an oversight. A fold pins the segment list
                    // and releases the lock, so a merge can unlink an input —
                    // and its sidecar — while a fold that pinned it is still
                    // working. Evicting the input's rollup at that moment
                    // takes away the last cheap copy: the fold then rebuilds
                    // it by decoding a segment whose sidecar no longer exists,
                    // which measured as a 1.6 s outlier. Keeping the entry
                    // costs nothing that lasts, because every fold ends by
                    // retaining the cache against the live segment list and
                    // will reclaim these paths as soon as no fold needs them.
                    // The stale entries are also SAFE to serve: segments are
                    // immutable, so the rollup still describes exactly what
                    // that segment held.
                    self.install_cached_rollups(&input_paths, installed);
                    true
                }
                None => false,
            }
        };
        if !published {
            // Nothing was replaced, so nothing may look replaced. The orphans
            // go first and DURABLY, and only then the journal: a crash in the
            // other order — or an unlink that failed and was shrugged off —
            // leaves an output with nothing left to describe it, and the next
            // open loads it like any other segment. That is not a cosmetic
            // leak. An output holds only its own group's view of a key and
            // carries a higher id than every input, so one sitting beside
            // intact inputs shadows a newer version in a group whose output
            // never landed.
            //
            // Handles first: an output is opened file-backed as it is written,
            // and a platform that refuses to unlink an open file would fail
            // every removal below while the merge still holds them.
            drop(outputs);
            for name in &new_names {
                unlink_segment(&self.directory.join(name))?;
            }
            sync_directory(&self.directory)?;
            fs::remove_file(&journal)?;
            sync_directory(&self.directory)?;
            return written.map(|()| 0);
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
        // Durable before the journal is dropped: the journal is what lets
        // recovery finish an unlink that a crash rolled back, so it must
        // outlive any unlink that is not yet durable. An unlink that fails
        // outright leaves the journal in place by the `?` above, and recovery
        // finishes the deletion from there.
        sync_directory(&self.directory)?;
        let _ = fs::remove_file(&journal);
        sync_directory(&self.directory)?;
        // Recorded only on the path where outputs were actually published, so
        // the counter means "merges that happened" rather than "merges that
        // were attempted". A tick that found nothing, or one whose run went
        // stale before it could publish, is not a merge.
        self.metrics.segment_merge.record(elapsed_nanos(&merging));
        self.metrics.segment_merges.increment();
        self.metrics
            .segments_merged_away
            .add(input_paths.len() as u64);
        Ok(input_paths.len().saturating_sub(new_names.len()))
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
        self.expire_by_policy_locked(&ExpiryCutoffs::single(cutoff_ns))
    }

    /// Removes a segment whose every span has expired.
    ///
    /// The same sequence the rewrite path uses when nothing survives, factored
    /// out so the fast path — which reaches this conclusion from the rollup's
    /// end-time range instead of by decoding — cannot drift from it. Off disk
    /// first, out of the live list second, cache last: a failed unlink must
    /// leave the store still reporting a segment that is still there, with
    /// something for the retry to find.
    fn retire_expired_segment(&self, segment: &std::sync::Arc<Segment>) -> Result<()> {
        unlink_segment(&segment.path)?;
        // An unlink is visible immediately and durable only when the directory
        // entry it removed is synced. Reporting the deletion before that would
        // let a crash bring the file — and the spans TTL just removed — back.
        sync_directory(&self.directory)?;
        let mut segments = self.lock_segments()?;
        if let Some(position) = segments
            .iter()
            .position(|held| std::sync::Arc::ptr_eq(held, segment))
        {
            segments.remove(position);
        }
        // Under the same guard as the removal, for the reason spelled out in
        // `expire_before_locked`: a reader that sees the new segment list
        // beside the old rollup reports spans TTL has just deleted.
        self.replace_cached_rollups(std::slice::from_ref(&segment.path), []);
        Ok(())
    }

    /// [`Self::expire_before`] with the maintenance lock already held,
    /// generalized to per-tenant cutoffs. Every predicate asks the POLICY,
    /// never a single number: a tenant with no window keeps its spans while
    /// its neighbours expire, and the segment fast paths use the policy's
    /// conservative bounds (see [`ExpiryCutoffs`]) so no whole-segment
    /// decision can ever be wrong for a tenant the bound did not consider.
    fn expire_by_policy_locked(&self, cutoffs: &ExpiryCutoffs) -> Result<usize> {
        // A seal in flight drained its spans BEFORE this deletion ran and
        // publishes its segment AFTER, so without this permit an expiry could
        // clean the buffer, the log and every segment it knew about, and then
        // watch a segment land holding exactly the spans it just deleted.
        // Waiting for the permit also keeps a new seal from starting until the
        // deletion is complete. Most ingest is unaffected: it never takes this
        // lock, and its seals simply coalesce into the next one. Two paths do
        // wait, and this permit is held for a whole deletion rather than the
        // microseconds compaction needs, so the wait is real —
        // [`Durability::Flushed`] must seal before it acknowledges, and any
        // mode blocks once the buffer reaches four times `flush_spans` (see
        // `seal_must_not_be_skipped`).
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
        let expired_span = |span: &Span| {
            cutoffs
                .cutoff_for(&span.tenant)
                .is_some_and(|cutoff_ns| span.end_time_ns < cutoff_ns)
        };
        let mut removed = {
            let mut writer = self.lock_writer()?;
            let expired = writer
                .spans
                .iter()
                .filter(|span| expired_span(span))
                .count();
            if expired > 0 {
                if let Some(log) = &self.wal {
                    // Borrowed, not cloned: the frame is serialized from
                    // references, so computing the survivors costs pointers
                    // rather than a copy of the buffer.
                    let survivors: Vec<&Span> = writer
                        .spans
                        .iter()
                        .filter(|span| !expired_span(span))
                        .map(|span| span.as_ref())
                        .collect();
                    log.rewrite(&survivors, 0)?;
                }
                writer.retain(|span| !expired_span(span));
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
        let pinned: Vec<std::sync::Arc<Segment>> = self.pin_segments()?;
        for segment in &pinned {
            // Decide whether this segment can hold anything expirable WITHOUT
            // decoding it.
            //
            // The sweep runs once a minute over every segment, and it used to
            // JSON-decode all of them every time — the entire corpus, to
            // discover that nothing had aged out. The rollup's end-time range
            // answers the question outright: entirely newer than the cutoff
            // means nothing expires, entirely older means everything does.
            // Only a segment straddling the cutoff has to be read.
            //
            // `None` means the sidecar is absent, stale or damaged, and the
            // only safe reading of that is "I do not know" — so it falls
            // through to the decode, which is what this did unconditionally
            // before. Retention is never decided on an unverified byte.
            let bounds = rollup_file::bounds(
                &segment.path,
                segment.rollup_binding(self.pricing_fingerprint()),
            );
            // Skip: no span can be older than the LATEST configured cutoff,
            // so no policy — however short its window — expires anything
            // here. Always sound; it only ever declines to decode less.
            if bounds.is_some_and(|bounds| bounds.min_end_ns >= cutoffs.latest()) {
                self.metrics.expiry_segments_skipped.increment();
                continue;
            }
            // Retire-whole: sound ONLY against a bound every span is subject
            // to. A tenant with no window has no cutoff at all, so this arm
            // exists only when the global TTL covers everyone
            // (`retire_bound` is `None` otherwise), and it tests the
            // EARLIEST cutoff — the longest window any tenant was promised.
            if cutoffs
                .retire_bound()
                .is_some_and(|bound| bounds.is_some_and(|bounds| bounds.max_end_ns < bound))
                && !segment.seg.is_empty()
            {
                // Every span in it is expired, so there is nothing to keep and
                // nothing to read: the whole segment goes.
                self.metrics.expiry_segments_skipped.increment();
                removed += segment.record_count();
                self.retire_expired_segment(segment)?;
                continue;
            }

            self.metrics.expiry_segments_decoded.increment();
            let all = segment.spans_parsed()?;
            let total = all.len();
            let mut kept: Vec<Span> = all.into_iter().filter(|span| !expired_span(span)).collect();
            if kept.len() == total {
                continue;
            }
            removed += total - kept.len();
            sort_spans(&mut kept);

            let replacement = match kept.is_empty() {
                true => None,
                false => Some(self.rewrite_segment_in_place(&segment.path, &kept)?),
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
            // Split the sealed result once: the segment goes into the live
            // list and its rollup into the cache, and neither step needs to
            // reopen the file that was just written.
            let (replacement, rollup) = match replacement {
                Some(sealed) => {
                    let binding = sealed.segment.rollup_binding(self.pricing_fingerprint());
                    (
                        Some(std::sync::Arc::new(sealed.segment)),
                        Some((binding, sealed.rollup)),
                    )
                }
                None => (None, None),
            };
            // Publish this one before rewriting the next, so an I/O failure
            // partway through leaves everything already rewritten correctly
            // represented rather than stranded. Nothing below can fail.
            //
            // The rollup cache is updated INSIDE this critical section, and
            // that is a correctness requirement rather than tidiness. Rollups
            // are keyed by path, and this path now holds different bytes — a
            // cached rollup would still count the spans TTL just deleted.
            // Updating it after releasing the guard leaves a window in which
            // a query sees the NEW segment beside the OLD rollup, and
            // `fold_analytics` takes the segments lock and then the rollups
            // lock, so it can land exactly there and report expired spans as
            // live. Deletion that a concurrent reader undoes is not deletion.
            // Taking rollups under segments is the order `fold_analytics`
            // already uses, so this adds no new lock edge.
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
                // Where a replacement was written its rollup is already built
                // and correct for the new bytes, so the stale entry is
                // REPLACED rather than merely dropped; where the segment went
                // away entirely there is nothing to install.
                let evicted = [segment.path.clone()];
                match rollup {
                    Some(rollup) => {
                        self.replace_cached_rollups(&evicted, [(segment.path.clone(), rollup)])
                    }
                    None => self.replace_cached_rollups(&evicted, []),
                }
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

    /// Number of segment rollups held in the analytics cache.
    ///
    /// An inspection hook, and the only view of a structure that is otherwise
    /// invisible to every resident-memory accessor on this type: a rollup
    /// costs eight bytes per span for the supersede prefilter plus its
    /// per-session trace sets, so a cache that fills up behind an operator's
    /// back is real memory. It exists so tests can assert WHAT fills it —
    /// notably that a maintenance timer does not, since a sweep touches every
    /// segment in the corpus including ones no query has ever asked about.
    pub fn cached_rollup_count(&self) -> Result<usize> {
        Ok(self
            .rollups
            .lock()
            .map_err(|_| Error::LockPoisoned("rollups"))?
            .len())
    }

    /// Number of fully materialized `Span` structs held for PERSISTED data.
    ///
    /// The segment memory rule: this is zero after open and flush. Segments hold
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

    /// Bytes the content index holds resident across all segments: the
    /// per-segment summary filters only.
    ///
    /// The per-block filters that do the real narrowing stay on disk and are
    /// read a row at a time, so this figure scales with SEGMENT COUNT and not
    /// with how much text the store holds. That is the property that makes
    /// content search compatible with a store larger than RAM, and this is the
    /// number that would betray it if it stopped being true.
    pub fn resident_content_index_bytes(&self) -> Result<usize> {
        let segments = self.lock_segments()?;
        Ok(segments
            .iter()
            .map(|segment| segment.seg.content_resident_bytes())
            .sum())
    }

    /// Mean fill ratio of the resident content summary filters, or `None` when
    /// no segment carries a content index.
    ///
    /// A value approaching 1.0 means the filters have saturated: they still
    /// cannot return a wrong answer, but they have stopped skipping segments
    /// and content search has quietly degraded to a scan. Nothing in a query's
    /// results would show that.
    pub fn content_summary_fill(&self) -> Result<Option<f64>> {
        let segments = self.lock_segments()?;
        let fills: Vec<f64> = segments
            .iter()
            .filter_map(|segment| segment.seg.content_summary_fill())
            .collect();
        if fills.is_empty() {
            return Ok(None);
        }
        Ok(Some(fills.iter().sum::<f64>() / fills.len() as f64))
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

    /// Acquires the segments lock, timing the wait.
    ///
    /// Timed at the acquisition rather than at chosen call sites, because the
    /// question this answers — "is something holding segments long enough to
    /// stall everyone else" — is not askable from any one caller. A seal takes
    /// writer then segments, so a reader holding segments stalls ingest, and
    /// the seal's own timers start after ITS locks are acquired: the wait
    /// showed up as a slow seal rather than as contention.
    fn lock_segments(&self) -> Result<MutexGuard<'_, Vec<std::sync::Arc<Segment>>>> {
        let waited = Instant::now();
        let guard = self
            .segments
            .lock()
            .map_err(|_| Error::LockPoisoned("segments"))?;
        self.metrics
            .segments_lock_wait
            .record(elapsed_nanos(&waited));
        Ok(guard)
    }

    /// The segment list as an owned snapshot, with the lock released.
    ///
    /// `Arc<Segment>` is a pointer, so this is a cheap copy — and a pinned
    /// segment keeps its own file descriptor, so it stays readable even if a
    /// merge or expiry unlinks it underneath. That is what lets a long read
    /// run without holding the lock every writer needs.
    fn pin_segments(&self) -> Result<Vec<std::sync::Arc<Segment>>> {
        Ok(self.lock_segments()?.clone())
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
        // Age is checked here as well as in `maintain_buffer` so an actively
        // ingesting store seals on the next batch rather than waiting out the
        // maintenance cadence; the tick exists for the store that goes quiet.
        if let (Some(limit), Some(oldest)) = (self.config.max_buffer_age, writer.oldest_at) {
            if oldest.elapsed() >= limit {
                return true;
            }
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
    /// Returns whether a segment was actually published: `false` when the
    /// seal coalesced into one already running or found the buffer empty, so
    /// a caller attributing seals to a trigger counts events, not attempts.
    fn seal(&self, wait: SealWait) -> Result<bool> {
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
                    return Ok(false);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Error::LockPoisoned("sealing"))
                }
            },
        };
        Ok(self.seal_with_permit()?.published)
    }

    /// [`Self::seal`] with the seal permit already held.
    fn seal_with_permit(&self) -> Result<SealOutcome> {
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
                return Ok(SealOutcome {
                    published: false,
                    folded: None,
                });
            }
            // Captured before anything else: with the writer lock held no
            // append can interleave, so every stamped frame at or before this
            // position carries a span this drain is about to copy out.
            let position = match &self.wal {
                Some(log) => Some(log.position()?),
                None => None,
            };
            // Cloning a `Vec<Arc<Span>>` copies pointers, not spans. This is
            // why the buffer holds handles: the same drain over `Vec<Span>`
            // would deep-copy ten thousand spans under the lock and give back
            // a good part of what moving the write off it just bought.
            let spans = writer.spans.clone();
            let upserts = writer.upserts;
            // Claimed under the segments lock, which is what orders it against
            // a concurrent compaction: see the doc comment above and
            // `merge_tail_run`. What keeps this id ordered against a merge's
            // is the seal permit this thread already holds — a merge claims
            // its own ids only while holding it, so it cannot be between this
            // drain and its publish.
            let _segments = self.lock_segments()?;
            let id = self.next_segment.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .segment_seal_locked
                .record(elapsed_nanos(&locked));
            Drained {
                spans,
                upserts,
                id,
                position,
            }
        };

        // ---- write: no engine lock held ---------------------------------
        let mut pending = drained.spans;
        pending.sort_by(|left, right| compare_spans(left, right));
        let written = self.write_segment(drained.id, &pending);
        // A freshly sealed segment's rollup is deliberately NOT cached here.
        // Nothing was evicted to make room for it, and a store that never
        // runs an aggregation should not accumulate an analytics cache; the
        // sidecar on disk already means the first query that does want it
        // reads a file instead of decoding a segment. See
        // `replace_cached_rollups`.
        let segment = match written {
            Ok(sealed) => sealed.segment,
            Err(error) => {
                // Nothing was published and nothing was removed — the buffer
                // and the log still hold every acknowledged span exactly as
                // they did before. A failed seal is a no-op, retried by the
                // next one.
                return Err(error);
            }
        };

        // ---- publish and reconcile: short critical sections --------------
        {
            let mut segments = self.lock_segments()?;
            let locked = Instant::now();
            segments.push(std::sync::Arc::new(segment));
            segments.sort_by(|left, right| left.path.cmp(&right.path));
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
        Ok(SealOutcome {
            published: true,
            folded: drained.position,
        })
    }

    /// Publishes a new generation: one manifest naming every load-bearing
    /// engine file with its digest, the log position it folded through, and
    /// one `CURRENT` rename — made durable by a directory fsync — that is the
    /// single commit point. Returns the new generation id.
    ///
    /// The sequence and its crash behaviour follow the checkpoint matrix in
    /// the design: everything before the `CURRENT` fsync is retryable and
    /// invisible (the old generation stays live, orphaned manifests are
    /// overwritten by the retry), and everything after it — advancing the
    /// stamp epoch, rolling the folded frames out of the log, sweeping old
    /// manifests — is housekeeping that a crash merely postpones. Recovery
    /// discards folded frames by stamp, trimmed or not.
    ///
    /// Holds the maintenance lock (no compaction or expiry may replace files
    /// mid-digest) and the seal permit (no segment may appear mid-manifest).
    /// Ingest keeps flowing throughout: batches admitted after the fold point
    /// stamp after it and replay against this generation on recovery.
    /// Annotation appends land past the manifested prefix, which verification
    /// reads as appends rather than damage.
    pub fn checkpoint(&self) -> Result<u64> {
        let _maintenance = self.lock_maintenance()?;
        let _permit = self
            .sealing
            .lock()
            .map_err(|_| Error::LockPoisoned("sealing"))?;

        // Seal what the buffer holds, capturing the fold point at the drain.
        // An empty buffer means nothing was drained: capture the position
        // under a fresh writer lock instead, re-checking emptiness so a batch
        // that slipped in between is sealed rather than silently folded.
        let folded = loop {
            let outcome = self.seal_with_permit()?;
            if let Some(folded) = outcome.folded {
                break folded;
            }
            match &self.wal {
                None => break generation::FoldedThrough::NONE,
                Some(log) => {
                    let writer = self.lock_writer()?;
                    if writer.is_empty() {
                        break log.position()?;
                    }
                    // Admitted between the empty drain and this lock: go
                    // around and seal it.
                }
            }
        };

        let epoch_floor = match &self.wal {
            Some(log) => log.position()?.epoch,
            None => 0,
        };
        let next = self
            .live_generation
            .load(Ordering::SeqCst)
            .max(epoch_floor)
            .saturating_add(1);

        let prior = generation::load_manifest(&generation::manifest_path(
            &self.directory,
            self.live_generation.load(Ordering::SeqCst),
        ))
        .map(|manifest| manifest.files)
        .unwrap_or_default();
        let files = generation::digest_engine(&self.directory, &prior)?;
        let created_unix_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
        let manifest = generation::Manifest {
            generation: next,
            created_unix_ns,
            folded_through: folded,
            files,
        };
        // The manifest is durable before CURRENT moves, or a restart would
        // point at a generation whose contents are not proven.
        generation::write_manifest(&self.directory, &manifest)?;
        generation::publish_current(&self.directory, next)?;
        self.live_generation.store(next, Ordering::SeqCst);

        // Everything past the CURRENT fsync is housekeeping: frames at or
        // before the fold are excluded by stamp whether or not this runs.
        if let Some(log) = &self.wal {
            log.advance_epoch(next)?;
            let writer = self.lock_writer()?;
            let survivors: Vec<&Span> = writer.spans.iter().map(|span| span.as_ref()).collect();
            if survivors.is_empty() {
                log.reset()?;
            } else {
                log.rewrite(&survivors, next)?;
            }
        }
        let _ = generation::sweep_generations(&self.directory, next);
        Ok(next)
    }

    /// The generation `CURRENT` currently names.
    pub fn live_generation(&self) -> u64 {
        self.live_generation.load(Ordering::SeqCst)
    }

    /// Re-reads a generation's manifest and checks every file's length and
    /// digest, returning each discrepancy. An empty vector means intact.
    ///
    /// This is what lets recovery distinguish "damage I may safely ignore"
    /// from "damage that changes what the store contains" by asking rather
    /// than inferring it from whether parsing happened to succeed. Pass the
    /// generation `CURRENT` names ([`Self::live_generation`]) to verify the
    /// live store; pass an older id to verify a generation a pin still holds.
    pub fn verify_generation(&self, generation: u64) -> Result<Vec<String>> {
        let manifest =
            generation::load_manifest(&generation::manifest_path(&self.directory, generation))?;
        // Serialize against the operations that replace files, so a digest is
        // never read across a rewrite. Reads and ingest continue.
        let _maintenance = self.lock_maintenance()?;
        generation::verify_against(&self.directory, &manifest)
    }

    /// Pins the live generation into `pins/<label>/` as a hard-link farm, so a
    /// backup can copy it while ingest and compaction carry on underneath.
    ///
    /// Checkpoints first, so the pin references an exact, digested manifest
    /// rather than a store mid-flight. Then, under the maintenance lock and
    /// seal permit — nothing may replace or add a file across the linking —
    /// every IMMUTABLE manifested file is hard-linked into the pin directory,
    /// the append-only logs are copied to their manifested prefix (a shared
    /// inode would let every later append edit the "backup" in place), and
    /// the manifest is copied beside them. Hard links share inodes, so the
    /// pin costs almost no disk and holds its bytes even after compaction
    /// unlinks the originals; the reader drops them when it removes the pin.
    /// Returns the pinned generation id.
    ///
    /// Backup is then: `pin`, [`verify_pin`](Self::verify_pin), copy the
    /// directory, [`release_pin`](Self::release_pin) — no server stop.
    pub fn pin_generation(&self, label: &str) -> Result<u64> {
        if label.is_empty() || label.contains(['/', '\\']) || label.starts_with('.') {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a pin label must be a single non-hidden path component",
            )));
        }
        let generation = self.checkpoint()?;
        let manifest =
            generation::load_manifest(&generation::manifest_path(&self.directory, generation))?;

        let _maintenance = self.lock_maintenance()?;
        let _permit = self
            .sealing
            .lock()
            .map_err(|_| Error::LockPoisoned("sealing"))?;

        let pin_dir = self.directory.join(generation::PINS_DIR).join(label);
        if pin_dir.exists() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a pin named {label:?} already exists"),
            )));
        }
        let staged = pin_dir.with_file_name(format!(".{label}.pinning"));
        let _ = fs::remove_dir_all(&staged);
        let build = (|| -> Result<()> {
            for file in &manifest.files {
                let relative = file.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let source = self.directory.join(&relative);
                let target = staged.join(&relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Immutable files share their inode with the pin — that is
                // what makes a pin nearly free. The append-only logs must
                // NOT: a hard-linked log lets every append after the pin
                // mutate the backup in place, and a later erasure's records
                // then leak into a pin that still holds the erased spans —
                // a restored store claiming a deletion it does not contain.
                // Those two files are copied, bounded to the manifested
                // prefix, which is exactly what the pin's manifest digests.
                match generation::is_append_only(&file.path) {
                    true => generation::copy_prefix(&source, &target, file.bytes)?,
                    false => fs::hard_link(&source, &target)?,
                }
            }
            generation::write_pin_manifest(&staged, &manifest)?;
            Ok(())
        })();
        if let Err(error) = build {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
        fs::create_dir_all(self.directory.join(generation::PINS_DIR))?;
        fs::rename(&staged, &pin_dir)?;
        sync_directory(&self.directory.join(generation::PINS_DIR))?;
        Ok(generation)
    }

    /// Where a pin's hard-link farm lives — the directory a backup copies.
    pub fn pin_path(&self, label: &str) -> PathBuf {
        self.directory.join(generation::PINS_DIR).join(label)
    }

    /// Verifies a pin's files against the manifest copied into it — the check
    /// a backup runs before trusting the copy it is about to make.
    pub fn verify_pin(&self, label: &str) -> Result<Vec<String>> {
        let pin_dir = self.pin_path(label);
        let manifest = generation::load_manifest(&pin_dir.join(generation::MANIFEST_NAME))?;
        generation::verify_against(&pin_dir, &manifest)
    }

    /// Removes a pin, freeing the disk its unshared bytes were holding. Idempotent.
    pub fn release_pin(&self, label: &str) -> Result<()> {
        match fs::remove_dir_all(self.directory.join(generation::PINS_DIR).join(label)) {
            Ok(()) => sync_directory(&self.directory.join(generation::PINS_DIR)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// Erases a subject — a trace, a span, a session, or an offloaded
    /// payload — from every domain, and publishes the deletion.
    ///
    /// The sequence is the contract:
    ///
    /// 1. **Resolve.** The subject becomes the concrete span keys it covers,
    ///    recorded so the erasure is exact rather than a predicate re-argued
    ///    later.
    /// 2. **Tombstone.** The intent is appended to `tombstones.jsonl` and
    ///    fsynced BEFORE anything is removed. From here the subject is
    ///    invisible to every query (the pending mask), and a crash anywhere
    ///    later leaves a pending erasure the next open masks and
    ///    [`Self::resume_erasures`] finishes — never a half-deletion nothing
    ///    remembers.
    /// 3. **Purge.** The write buffer and log are rewritten to the survivors
    ///    (the same discipline as TTL: a deletion a restart undoes is not a
    ///    deletion); every segment holding a match is rewritten in place or
    ///    removed; annotations addressed to erased spans are dropped; payload
    ///    files are deleted **reference-aware** — content addressing means
    ///    one file can back spans outside the subject, and those bytes are
    ///    retained and reported rather than destroyed. Superseded versions
    ///    of erased spans purge with them: the purge tests every physical
    ///    record against the subject, not just the visible one.
    /// 4. **Publish.** A checkpoint moves `CURRENT`; the deletion is durable
    ///    at that rename, and the generation's manifest names the tombstone
    ///    log that commands it.
    /// 5. **Settle.** The outcome is appended and the mask lifts. New spans
    ///    under the same identifiers are new data from here on — an erasure
    ///    is a barrier, not a ban.
    ///
    /// Reads and ingest continue throughout, exactly as they do across TTL
    /// expiry. A reader that PINNED the store before the erasure (a running
    /// export) finishes against its pinned view — POSIX semantics; new reads
    /// cannot see the subject from step 2 on.
    ///
    /// What remains afterwards, on purpose: the tombstone record itself,
    /// naming the subject and its keys. That record is what
    /// [`Self::verify_erasure`] checks the store against; erasing the record
    /// of erasure means deleting the store.
    pub fn erase(&self, subject: erasure::Subject) -> Result<erasure::ErasureStatus> {
        // Canonicalize BEFORE validating or resolving: an uppercase payload
        // hash would pass a lenient validator, match no stored reference —
        // every downstream comparison is case-sensitive — and produce a
        // green receipt over content still fully present. The recorded
        // subject is always the canonical form.
        let subject = subject.canonicalized();
        subject.validate()?;
        // Payload subjects resolve BEFORE the gate: their resolution is a
        // fold over the whole store, and stalling every admission for a
        // corpus scan is not a price `begin` may charge. Their recorded
        // refs cannot diverge from the purge's — the subject reference IS
        // the ref list.
        let pre_resolved = match &subject {
            erasure::Subject::Payload { .. } => Some(self.resolve_subject(&subject)?),
            _ => None,
        };
        let requested_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
        // The mask moves only under the write half of the erasure gate:
        // every in-flight ingest or annotation admission holding the read
        // half completes first (its data lands pre-mask, where the purge
        // finds it), and everything arriving after sees the mask at every
        // step. This is what makes the barrier hold at the BEGIN transition,
        // not just in the steady pending state.
        //
        // Trace, span, session and tenant subjects RESOLVE in here too —
        // bounded lookups, unlike a payload fold. Resolving outside the
        // write half left a window where a span (and its payload refs)
        // joined the subject between resolution and `begin`: the purge
        // would then delete a reference the mask never accounted for, and
        // a promotion racing the sweep could commit an example pointing at
        // bytes the erasure was about to unlink. Under the write half no
        // admission is in flight and none can start, so what resolution
        // records is exactly what the purge will find.
        let record = {
            let _gate = self
                .erasure_gate
                .write()
                .map_err(|_| Error::LockPoisoned("erasure gate"))?;
            let (span_keys, payload_refs) = match pre_resolved {
                Some(resolved) => resolved,
                None => self.resolve_subject(&subject)?,
            };
            let (eval_datasets, eval_experiments) = match &subject {
                erasure::Subject::Tenant { tenant } => self.evals.ids_of_tenant(tenant)?,
                _ => (Vec::new(), Vec::new()),
            };
            self.erasures.begin(
                requested_unix_ns,
                subject,
                span_keys,
                payload_refs,
                eval_datasets,
                eval_experiments,
            )?
        };
        let settle = self.complete_erasure(&record)?;
        // Re-read so the caller sees the record as the log now holds it —
        // a tenant subject's payload refs were NOTED during the purge, not
        // recorded at begin, and the receipt (and any caller) should see
        // the complete accounting.
        let erase = self
            .erasures
            .get(record.id)?
            .map(|status| status.erase)
            .unwrap_or(record);
        Ok(erasure::ErasureStatus {
            erase,
            settle: Some(settle),
        })
    }

    /// Finishes every pending erasure — one whose purge a crash interrupted —
    /// and returns how many were settled. The purge steps are idempotent, so
    /// resuming an erasure that was nearly done merely re-verifies each
    /// domain and publishes. `traza-server` calls this from its maintenance
    /// tick; an embedding process should call it once after open.
    pub fn resume_erasures(&self) -> Result<usize> {
        let pending = self.erasures.pending()?;
        for record in &pending {
            self.complete_erasure(record)?;
        }
        Ok(pending.len())
    }

    /// Every erasure the tombstone log records, oldest first.
    pub fn erasures(&self) -> Result<Vec<erasure::ErasureStatus>> {
        self.erasures.list()
    }

    /// One erasure by id, or `None` when the log never recorded it.
    pub fn erasure_status(&self, id: u64) -> Result<Option<erasure::ErasureStatus>> {
        self.erasures.get(id)
    }

    /// The subject resolved to the concrete keys and payload references it
    /// covers right now, under the store's usual read semantics. Trace and
    /// span subjects resolve UNDER THEIR TENANT'S SCOPE — an unscoped lookup
    /// would let another tenant's newer same-id span shadow the subject's
    /// own out of the resolved set, and the receipt's re-delivery check
    /// would then have nothing to catch a re-delivery against.
    fn resolve_subject(&self, subject: &erasure::Subject) -> Result<ResolvedSubject> {
        let spans: Vec<Span> = match subject {
            erasure::Subject::Trace { trace_id, tenant } => {
                self.get_trace_in(Some(tenant), trace_id)?
            }
            erasure::Subject::Span {
                trace_id,
                span_id,
                tenant,
            } => self
                .get_trace_in(Some(tenant), trace_id)?
                .into_iter()
                .filter(|span| span.span_id == *span_id)
                .collect(),
            erasure::Subject::Session { session_id, tenant } => {
                self.resolve_session_spans(Some(tenant), session_id)?
            }
            // A tenant's span set is unbounded, and the mask covers it by
            // predicate. Resolving it into the record would serialize the
            // store into one tombstone line under the gate's write half —
            // and the purge collects keys and refs as it walks anyway.
            erasure::Subject::Tenant { .. } => Vec::new(),
            erasure::Subject::Payload { reference } => {
                // No index reaches into reference objects (their hashes are
                // deliberately not content-indexed), so resolution is a fold.
                // Payload erasure is an explicit administrative act; one scan
                // per request is the honest price.
                let mut matching = Vec::new();
                let view = self.snapshot()?;
                view.fold(
                    &SpanFilter::default(),
                    &mut QueryCost::default(),
                    &mut |span| {
                        if erasure::payload_unredacted(span, reference) {
                            matching.push(span.clone());
                        }
                    },
                )?;
                matching
            }
        };
        let mut keys: Vec<(String, String, String)> = spans
            .iter()
            .map(|span| {
                (
                    span.tenant.clone(),
                    span.trace_id.clone(),
                    span.span_id.clone(),
                )
            })
            .collect();
        keys.sort();
        keys.dedup();
        let mut refs: std::collections::HashSet<String> = std::collections::HashSet::new();
        match subject {
            erasure::Subject::Payload { reference } => {
                refs.insert(reference.clone());
            }
            // A tenant subject records NO payload refs at begin: enumerating
            // them is a corpus fold, and a fold under the gate's write half
            // would stall every tenant's ingest for its duration. They are
            // resolved instead in `complete_erasure`, under the maintenance
            // lock with the gate released — where the mask has already
            // frozen the tenant's spans, so the set is complete and stable —
            // and durably NOTED so a resumed erasure can still reconstruct
            // the payload sweep after a crash.
            erasure::Subject::Tenant { .. } => {}
            _ => {
                for span in &spans {
                    erasure::payload_refs_of(span, &mut refs);
                }
            }
        }
        let mut refs: Vec<String> = refs.into_iter().collect();
        refs.sort();
        Ok((keys, refs))
    }

    /// Runs the purge, publishes, and settles one recorded erasure. Shared by
    /// [`Self::erase`] and [`Self::resume_erasures`]; every step is
    /// idempotent, so running it again after a crash re-verifies rather than
    /// re-damages.
    fn complete_erasure(&self, record: &erasure::EraseRecord) -> Result<erasure::SettleRecord> {
        let erasing = Instant::now();
        let subject = &record.subject;

        let (purge, annotations_removed, eval_records_removed, payloads_removed, payloads_retained) = {
            // One rewriter at a time, same as compaction and expiry.
            let _maintenance = self.lock_maintenance()?;

            // A tenant subject's payload refs are resolved HERE, not at
            // begin: the mask (installed at begin) has already frozen the
            // tenant's spans, so the set is complete and stable, and the gate
            // is released so no other tenant's ingest is blocked by the walk.
            // The refs are noted durably BEFORE the purge removes the spans
            // and bodies that carry them — that is what lets a resume after a
            // crash mid-purge still reconstruct which bytes to unlink, from
            // the intent record alone.
            //
            // The scan is MASK-FREE by construction: it reads raw buffer and
            // segment spans, not through a query's `visible` filter. A masked
            // read would see NOTHING — the tenant mask covers every one of
            // the tenant's own spans — so the span-carried refs would silently
            // never be recorded, and only the transient purge output would
            // hold them, orphaning a span-held payload on a crash-resume.
            if let erasure::Subject::Tenant { tenant } = subject {
                let mut refs = self.tenant_span_payload_refs(tenant)?;
                refs.extend(self.evals.tenant_payload_refs(tenant)?);
                let mut refs: Vec<String> = refs.into_iter().collect();
                refs.sort();
                self.erasures.note_payload_refs(record.id, refs)?;
            }
            // Re-read the record so the notes just written (and any from a
            // prior interrupted pass) are in `record.payload_refs` — the
            // durable source the doomed set derives from.
            let record = self
                .erasures
                .get(record.id)?
                .map(|status| status.erase)
                .unwrap_or_else(|| record.clone());
            let record = &record;
            let purge = self.purge_subject_locked(subject)?;

            let annotations_removed = self.drop_annotations_for(subject, &purge.keys)?;
            // A tenant subject's eval records leave inside the same barrier,
            // before the checkpoint the settle will cite — the eval log is
            // manifested, and nothing may rewrite a manifested file past
            // that checkpoint. The refs its dropped bodies held join the
            // doomed set below: content whose ONLY holder was the erased
            // tenant's examples must lose its bytes too, not linger under a
            // receipt with nothing to check.
            let (eval_records_removed, eval_dropped_refs) = match subject {
                erasure::Subject::Tenant { tenant } => self.evals.purge_tenant(tenant)?,
                _ => (0, std::collections::HashSet::new()),
            };

            // Payload files, reference-aware. Live references are computed
            // AFTER the span purge, so a file only the subject referenced is
            // sweepable and a file shared with surviving spans is not.
            let mut payloads_removed: Vec<String> = Vec::new();
            let mut payloads_retained: Vec<erasure::RetainedPayload> = Vec::new();
            let mut doomed: Vec<String> = record
                .payload_refs
                .iter()
                .cloned()
                .chain(purge.payload_refs.iter().cloned())
                .chain(eval_dropped_refs.iter().cloned())
                .collect();
            doomed.sort();
            doomed.dedup();
            if !doomed.is_empty() {
                let erased_file = |reference: &str| -> Result<bool> {
                    let Some(hash) = reference.strip_prefix("sha256/") else {
                        return Ok(false);
                    };
                    match fs::remove_file(payload::payload_path(&self.directory, hash)) {
                        Ok(()) => Ok(true),
                        // Already absent is the erased state; resume hits this.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                        Err(error) => Err(Error::Io(error)),
                    }
                };
                match subject {
                    erasure::Subject::Payload { .. } => {
                        // The content itself is the subject: every reference
                        // was redacted above, so the file goes regardless of
                        // who pointed at it.
                        for reference in doomed {
                            erased_file(&reference)?;
                            payloads_removed.push(reference);
                        }
                    }
                    _ => {
                        // Live references are the ONLY retention ground here,
                        // and deliberately not the touch registry the TTL
                        // sweep also consults. The two tools resolve the same
                        // race in opposite directions, each rightly: retention
                        // must never destroy a span's data, so TTL yields to
                        // any recent toucher; an erasure was COMMANDED to
                        // destroy this data, so it yields only to spans that
                        // provably still reference the bytes. The residual
                        // race is one in-flight ingest re-uploading the exact
                        // content being erased: if it has not yet passed its
                        // existence check it recreates the file (the standard
                        // store_payload interlock), and if it has, its span
                        // keeps the inline preview while the offloaded body
                        // defers to the erasure — the priority an erasure
                        // exists to enforce.
                        let live = self.live_payload_refs()?;
                        for reference in doomed {
                            if live.contains(&reference) {
                                payloads_retained.push(erasure::RetainedPayload {
                                    reference,
                                    reason: "still referenced by live spans outside the subject"
                                        .to_owned(),
                                });
                                continue;
                            }
                            erased_file(&reference)?;
                            payloads_removed.push(reference);
                        }
                    }
                }
                // An unlink is durable only when its directory entry is
                // synced, and payload files live in SHARD directories — the
                // parent alone would not carry the removals. Deduplicated,
                // because many removals share a shard.
                let mut shards: Vec<PathBuf> = payloads_removed
                    .iter()
                    .filter_map(|reference| reference.strip_prefix("sha256/"))
                    .filter_map(|hash| {
                        payload::payload_path(&self.directory, hash)
                            .parent()
                            .map(Path::to_path_buf)
                    })
                    .collect();
                shards.sort();
                shards.dedup();
                for shard in shards {
                    if shard.exists() {
                        sync_directory(&shard)?;
                    }
                }
            }
            (
                purge,
                annotations_removed,
                eval_records_removed,
                payloads_removed,
                payloads_retained,
            )
        };

        // The live tail must stop serving what the store no longer holds. A
        // veil covers exactly the entries admitted before this point; spans
        // admitted later are new data and flow normally. A tenant subject
        // resolves no keys — its veil is the tenant predicate itself.
        self.veil_tail(subject, &purge.keys);

        // ---- confirm: everything mutable, BEFORE the checkpoint ----------
        // The admission barrier suppresses covered spans (and the annotate
        // barrier covered annotations) from the moment the intent record
        // installed the mask, so nothing covered can have been ADMITTED
        // since. This pass sweeps what the barriers structurally cannot:
        // batches already inside the writer lock when the mask landed, any
        // segment a concurrent seal published from them, and — for a payload
        // subject — a file an offload racing `begin` may have recreated. On
        // the common path it finds nothing and costs index probes.
        //
        // It runs BEFORE the checkpoint because the settle record names that
        // checkpoint's generation, and a generation is a set of digests:
        // rewriting the annotation log or a segment AFTER publishing it
        // would make the very generation the receipt cites fail its own
        // verification. Order is barrier → purge → confirm → checkpoint →
        // settle → the mask lifts; nothing rewrites a manifested file past
        // the checkpoint, and the settle append itself rides the append-only
        // allowance every manifest already grants the tombstone log.
        let confirm = {
            let _maintenance = self.lock_maintenance()?;
            let confirm = self.purge_subject_locked(subject)?;
            if let Some(reference) = subject.payload_reference() {
                if let Some(hash) = reference.strip_prefix("sha256/") {
                    // Idempotent re-delete: closes the window where an
                    // offload that loaded the mask just before `begin` wrote
                    // the subject's bytes back mid-purge.
                    match fs::remove_file(payload::payload_path(&self.directory, hash)) {
                        Ok(()) => sync_directory(
                            payload::payload_path(&self.directory, hash)
                                .parent()
                                .unwrap_or(&self.directory),
                        )?,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(Error::Io(error)),
                    }
                }
            }
            confirm
        };
        if !confirm.keys.is_empty() {
            self.veil_tail(subject, &confirm.keys);
        }
        // Annotations that arrived before the annotate barrier went up get
        // the same confirm treatment; a no-op rewrite costs nothing (the
        // rewrite returns before touching the file when nothing matches).
        // The eval log gets it too, for the same belt-and-braces reason,
        // and both are idempotent.
        let annotations_removed =
            annotations_removed + self.drop_annotations_for(subject, &confirm.keys)?;
        let eval_records_removed = eval_records_removed
            + match subject {
                erasure::Subject::Tenant { tenant } => self.evals.purge_tenant(tenant)?.0,
                _ => 0,
            };
        let spans_removed = purge.removed + confirm.removed;
        let spans_redacted = purge.redacted + confirm.redacted;

        // Publication: the deletion is durable when `CURRENT` moves, and the
        // manifest it moves to names the tombstone log that commands it and
        // digests every file the purge left behind. The checkpoint also
        // rewrites the log to the surviving buffer, so the erased bytes
        // leave `wal.log` here at the latest.
        let generation = self.checkpoint()?;

        // The settle lifts the mask, and with it the barriers — under the
        // WRITE half of the erasure gate, so it is ordered against every
        // in-flight admission exactly as `begin` was. The cut is exact
        // without holding any engine lock: no covered span can be admitted
        // while the mask is up (admissions hold the gate's read half across
        // their whole decision, so none can straddle the transition), the
        // ones admitted before the mask went up were swept by the purge and
        // confirm passes above, and anything admitted after this append is
        // post-settle new data whose acknowledgement follows
        // `settled_unix_ns` — and is stored, because no admission can be
        // mid-flight with a stale pending mask when the write half is held.
        let settle = erasure::SettleRecord {
            schema: erasure::SettleRecord::schema_now(),
            id: record.id,
            settled_unix_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64),
            generation,
            spans_removed,
            spans_redacted,
            annotations_removed,
            payloads_removed,
            payloads_retained,
            eval_records_removed,
        };
        {
            let _gate = self
                .erasure_gate
                .write()
                .map_err(|_| Error::LockPoisoned("erasure gate"))?;
            self.erasures.record_settle(settle.clone())?;
        }
        self.metrics.erasures_settled.increment();
        self.metrics.erasure_spans_removed.add(spans_removed as u64);
        self.metrics.erasure.record(elapsed_nanos(&erasing));
        Ok(settle)
    }

    /// Veils the live tail for one purge pass: by the pass's keys, and — for
    /// a tenant subject, whose keys are deliberately unresolved — by the
    /// tenant predicate, so ring entries admitted before the erasure stop
    /// being served even though no key list names them.
    fn veil_tail(
        &self,
        subject: &erasure::Subject,
        keys: &std::collections::HashSet<(String, String, String)>,
    ) {
        let tenants: std::collections::HashSet<String> = match subject {
            erasure::Subject::Tenant { tenant } => std::iter::once(tenant.clone()).collect(),
            _ => std::collections::HashSet::new(),
        };
        if keys.is_empty() && tenants.is_empty() {
            return;
        }
        self.tail.veil(
            std::sync::Arc::new(keys.clone()),
            std::sync::Arc::new(tenants),
        );
    }

    /// One purge pass's annotation drop, with the subject expressed in the
    /// annotation log's own typed terms: span keys always; the whole trace
    /// for a trace subject (trace-level annotations included); the whole
    /// session for a session subject (session-subject annotations have no
    /// span address for the keys to catch); the whole tenant for a tenant
    /// subject (scores included, whatever their shape). Payload subjects
    /// drop nothing — a redaction is not a deletion of judgment.
    fn drop_annotations_for(
        &self,
        subject: &erasure::Subject,
        keys: &std::collections::HashSet<(String, String, String)>,
    ) -> Result<usize> {
        match subject {
            erasure::Subject::Payload { .. } => Ok(0),
            erasure::Subject::Trace { trace_id, tenant } => {
                self.annotations
                    .drop_for_subject(keys, Some((tenant, trace_id)), None, None)
            }
            erasure::Subject::Session { session_id, tenant } => {
                self.annotations
                    .drop_for_subject(keys, None, Some((tenant, session_id)), None)
            }
            erasure::Subject::Tenant { tenant } => {
                self.annotations
                    .drop_for_subject(keys, None, None, Some(tenant))
            }
            // The span's OWN key, always — not only the keys the purge
            // resolved. A span-addressed score (a run's judgment) survives
            // its span's TTL expiry, so by the time the span is erased it
            // may be absent from the corpus and resolve to no keys; dropping
            // only resolved keys then left the score forever, and the
            // receipt read `holds-data` with no way to clear it.
            erasure::Subject::Span {
                trace_id,
                span_id,
                tenant,
            } => {
                let mut with_subject: std::collections::HashSet<(String, String, String)> =
                    keys.clone();
                with_subject.insert((tenant.clone(), trace_id.clone(), span_id.clone()));
                self.annotations
                    .drop_for_subject(&with_subject, None, None, None)
            }
        }
    }

    /// Removes (or redacts) every physical record the subject covers, across
    /// the buffer, the log, and every segment. The maintenance lock is held
    /// by the caller; the seal permit is taken here for exactly the reason
    /// expiry takes it — a seal that drained before the purge must not
    /// publish the purged spans back afterwards.
    fn purge_subject_locked(&self, subject: &erasure::Subject) -> Result<SubjectPurge> {
        let _permit = self
            .sealing
            .lock()
            .map_err(|_| Error::LockPoisoned("sealing"))?;
        let mut purge = SubjectPurge::default();

        // ---- buffer and log ---------------------------------------------
        // Durable state first, memory second, exactly as expiry: the log is
        // rewritten to the survivors before the buffer drops anything, so a
        // failed rewrite leaves the store as retryable as it found it.
        {
            let mut writer = self.lock_writer()?;
            let mut next: Vec<std::sync::Arc<Span>> = Vec::with_capacity(writer.spans.len());
            let mut changed = false;
            for span in writer.spans.iter() {
                match subject.action(span) {
                    erasure::Action::Keep => next.push(std::sync::Arc::clone(span)),
                    erasure::Action::Drop => {
                        changed = true;
                        purge.removed += 1;
                        purge.keys.insert((
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        ));
                        erasure::payload_refs_of(span, &mut purge.payload_refs);
                    }
                    erasure::Action::Redact => {
                        changed = true;
                        purge.redacted += 1;
                        purge.keys.insert((
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        ));
                        let mut redacted = Span::clone(span);
                        if let Some(reference) = subject.payload_reference() {
                            erasure::redact_payload(&mut redacted, reference);
                        }
                        next.push(std::sync::Arc::new(redacted));
                    }
                }
            }
            if changed {
                if let Some(log) = &self.wal {
                    let survivors: Vec<&Span> = next.iter().map(|span| span.as_ref()).collect();
                    log.rewrite(&survivors, 0)?;
                }
                writer.restore(next);
            }
        }

        // ---- segments: pinned, rewritten with no engine lock held --------
        // Same shape as expiry, and for the same reasons: in-place renames
        // keep path order (which IS recency order), pinned readers keep their
        // descriptors, and each segment is published before the next is
        // touched so a failure strands nothing.
        let pinned: Vec<std::sync::Arc<Segment>> = self.pin_segments()?;
        for segment in &pinned {
            if !self.segment_may_hold_subject(segment, subject)? {
                continue;
            }
            self.metrics.expiry_segments_decoded.increment();
            let all = segment.spans_parsed()?;
            let total = all.len();
            let mut kept: Vec<Span> = Vec::with_capacity(total);
            let mut changed = false;
            for span in all {
                match subject.action(&span) {
                    erasure::Action::Keep => kept.push(span),
                    erasure::Action::Drop => {
                        changed = true;
                        purge.removed += 1;
                        purge.keys.insert((
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        ));
                        erasure::payload_refs_of(&span, &mut purge.payload_refs);
                    }
                    erasure::Action::Redact => {
                        changed = true;
                        purge.redacted += 1;
                        purge.keys.insert((
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        ));
                        let mut redacted = span;
                        if let Some(reference) = subject.payload_reference() {
                            erasure::redact_payload(&mut redacted, reference);
                        }
                        kept.push(redacted);
                    }
                }
            }
            if !changed {
                continue;
            }
            sort_spans(&mut kept);

            let replacement = match kept.is_empty() {
                true => None,
                false => Some(self.rewrite_segment_in_place(&segment.path, &kept)?),
            };
            if replacement.is_none() {
                unlink_segment(&segment.path)?;
                sync_directory(&self.directory)?;
            }
            let (replacement, rollup) = match replacement {
                Some(sealed) => {
                    let binding = sealed.segment.rollup_binding(self.pricing_fingerprint());
                    (
                        Some(std::sync::Arc::new(sealed.segment)),
                        Some((binding, sealed.rollup)),
                    )
                }
                None => (None, None),
            };
            // Publish under the segments guard, rollup cache inside the same
            // critical section — the expiry path's rule, for the expiry
            // path's reason: a reader must never see the new segment beside
            // the old rollup.
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
                let evicted = [segment.path.clone()];
                match rollup {
                    Some(rollup) => {
                        self.replace_cached_rollups(&evicted, [(segment.path.clone(), rollup)])
                    }
                    None => self.replace_cached_rollups(&evicted, []),
                }
            }
        }
        Ok(purge)
    }

    /// Whether a segment could hold any record the subject covers, answered
    /// from indexes and sidecars where possible so an erasure does not decode
    /// the corpus. `true` means "decode and check", never "matches".
    fn segment_may_hold_subject(
        &self,
        segment: &Segment,
        subject: &erasure::Subject,
    ) -> Result<bool> {
        match subject {
            erasure::Subject::Trace { trace_id, .. } | erasure::Subject::Span { trace_id, .. } => {
                // The trace index over-approximates across tenants; the
                // decode-and-check that follows settles it (invariant 7).
                Ok(!segment.trace_spans(trace_id)?.is_empty())
            }
            erasure::Subject::Session { session_id, .. } => {
                for key in &semconv::SESSION_KEYS {
                    for value in analytics::session_values(session_id) {
                        if !attribute_candidates(&segment.seg, key, &value).is_empty() {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            erasure::Subject::Tenant { tenant } => {
                // The reserved tenant posting is written for every non-empty
                // tenant, so an empty candidate list IS proof of absence —
                // and tenant subjects cannot name the default tenant, whose
                // spans carry no posting. A span acquires an identity tenant
                // only from the reserved `$tenant` key (see [`Span::tenant`]),
                // which a tenant-aware build always writes alongside this
                // posting — so for every span THIS build wrote, a set tenant
                // implies a posting, and the fast path is exact.
                //
                // The one corpus it cannot see is a FOREIGN pre-tenancy store
                // whose span JSON already used a literal top-level `$tenant`
                // key for its own data: decoded here it becomes an identity
                // with no posting, which this probe would miss and a whole-
                // tenant erasure would then skip under a settle receipt. No
                // store of ours writes such bytes, and the pre-1.0 terms do
                // not promise reading foreign ones — a pre-tenancy import path
                // must fold such a key out at decode (see [`Span::tenant`])
                // before this invariant holds for it. It is a property of the
                // key for our own corpus, and an assumption about anyone
                // else's.
                Ok(!segment
                    .seg
                    .attribute_candidate_offsets(IDX_TENANT, tenant)
                    .is_empty())
            }
            erasure::Subject::Payload { reference } => {
                // The sidecar's reference set answers without a decode; a
                // segment with no usable sidecar cannot be ruled out.
                match rollup_file::load(
                    &segment.path,
                    segment.rollup_binding(self.pricing_fingerprint()),
                ) {
                    Some(rollup) => Ok(rollup.payload_refs.contains(reference)),
                    None => Ok(true),
                }
            }
        }
    }

    /// Every `(tenant, trace_id, span_id)` in `seg` the subject covers — the
    /// verification-side probe, over any segment file (live or pinned).
    /// For a payload subject a span counts only while it still carries an
    /// UNREDACTED reference: the redaction marker left behind is the erasure
    /// working as designed, not a finding.
    fn subject_keys_in_segment(
        seg: &segment::Segment,
        subject: &erasure::Subject,
    ) -> Result<Vec<(String, String, String)>> {
        let mut keys = Vec::new();
        let key_of = |span: Span| (span.tenant, span.trace_id, span.span_id);
        match subject {
            erasure::Subject::Trace { trace_id, tenant } => {
                for record in seg.query_trace(trace_id).map_err(segment_error)? {
                    let span = record_to_span(&record)?;
                    if span.tenant == *tenant {
                        keys.push(key_of(span));
                    }
                }
            }
            erasure::Subject::Span {
                trace_id,
                span_id,
                tenant,
            } => {
                for record in seg.query_trace(trace_id).map_err(segment_error)? {
                    let span = record_to_span(&record)?;
                    if span.span_id == *span_id && span.tenant == *tenant {
                        keys.push(key_of(span));
                    }
                }
            }
            erasure::Subject::Session { session_id, tenant } => {
                let mut offsets: Vec<u64> = Vec::new();
                for key in &semconv::SESSION_KEYS {
                    for value in analytics::session_values(session_id) {
                        offsets.extend_from_slice(&attribute_candidates(seg, key, &value));
                    }
                }
                offsets.sort_unstable();
                offsets.dedup();
                for offset in offsets {
                    let record = seg.record_at_offset(offset).map_err(segment_error)?;
                    let span = record_to_span(&record)?;
                    if span.tenant == *tenant
                        && semconv::facts(&span.attributes).session.as_deref() == Some(session_id)
                    {
                        keys.push(key_of(span));
                    }
                }
            }
            erasure::Subject::Tenant { tenant } => {
                let offsets = seg.attribute_candidate_offsets(IDX_TENANT, tenant).to_vec();
                for offset in offsets {
                    let record = seg.record_at_offset(offset).map_err(segment_error)?;
                    let span = record_to_span(&record)?;
                    if span.tenant == *tenant {
                        keys.push(key_of(span));
                    }
                }
            }
            erasure::Subject::Payload { reference } => {
                for ordinal in 0..seg.len() {
                    if let Some(record) = seg.record(ordinal).map_err(segment_error)? {
                        let span = record_to_span(&record)?;
                        if erasure::payload_unredacted(&span, reference) {
                            keys.push(key_of(span));
                        }
                    }
                }
            }
        }
        Ok(keys)
    }

    /// Verifies one erasure against every domain the subject's bytes could
    /// inhabit, and returns the receipt: each domain named, each checked,
    /// each result stated. This is a VERIFICATION — the result field is
    /// computed from what the walk found, never from what the settle record
    /// claims.
    ///
    /// Advisory by design: it reads the live store without stopping it, so a
    /// span ingested mid-walk lands in whichever half of the walk reaches it.
    /// Matches are classified against the erase record's resolved keys — an
    /// erased key found live again is a re-delivery and fails the receipt; a
    /// fresh key matching the subject is new activity and is reported
    /// without failing it, because an erasure is a barrier, not a ban.
    pub fn verify_erasure(&self, id: u64) -> Result<erasure::Receipt> {
        let Some(status) = self.erasures.get(id)? else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no erasure {id} is recorded in the tombstone log"),
            )));
        };
        let record = &status.erase;
        let subject = &record.subject;
        let erased_keys: std::collections::HashSet<(String, String, String)> =
            record.span_keys.iter().cloned().collect();
        let needles = subject.needles();
        let mut domains: Vec<erasure::DomainReport> = Vec::new();
        let mut clean = status.settle.is_some();

        // A tenant subject records no span keys — its coverage is a
        // predicate, not a list — so the span domains classify every
        // resident tenant span as new activity and cannot fail on one.
        // That is CORRECT, not a gap: after settle the barrier and the
        // exhaustive mask-free purge guarantee no covered span was left, and
        // a span present at verify time is post-settle admission (an erasure
        // is a barrier, not a ban). Spans carry no admission-order field, so
        // there is no sound way to separate a purge survivor from a
        // backfilled new span by their client event time — a check on
        // `start_time_ns` would fail the receipt forever the first time a
        // delayed exporter delivered an old-timestamped span. The purge's
        // correctness is proven by its own tests and the raw match counts in
        // these domains' detail, not by a timestamp the client controls.

        // ---- write buffer -------------------------------------------------
        {
            let matches: Vec<(String, String, String)> = {
                let writer = self.lock_writer()?;
                writer
                    .spans
                    .iter()
                    .filter(|span| match subject {
                        erasure::Subject::Payload { reference } => {
                            erasure::payload_unredacted(span, reference)
                        }
                        _ => subject.action(span) != erasure::Action::Keep,
                    })
                    .map(|span| {
                        (
                            span.tenant.clone(),
                            span.trace_id.clone(),
                            span.span_id.clone(),
                        )
                    })
                    .collect()
            };
            let held = matches.len();
            let classes = erasure::classify_matches(&erased_keys, matches);
            clean &= classes.re_delivered == 0;
            let mut report = erasure::DomainReport::clear(
                "write-buffer",
                match held {
                    0 => "no buffered span matches the subject".to_owned(),
                    found => format!("{found} buffered span(s) match the subject"),
                },
            );
            report.re_delivered = classes.re_delivered;
            report.new_activity = classes.new_activity;
            if classes.re_delivered > 0 {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- live tail ------------------------------------------------------
        {
            let matches = self.tail.matching_keys(&|span| match subject {
                erasure::Subject::Payload { reference } => {
                    erasure::payload_unredacted(span, reference)
                }
                _ => subject.action(span) != erasure::Action::Keep,
            });
            let held = matches.len();
            let classes = erasure::classify_matches(&erased_keys, matches);
            clean &= classes.re_delivered == 0;
            let mut report = erasure::DomainReport::clear(
                "live-tail",
                match held {
                    0 => "no retained tail entry serves the subject".to_owned(),
                    found => format!("{found} retained tail entr(ies) match the subject"),
                },
            );
            report.re_delivered = classes.re_delivered;
            report.new_activity = classes.new_activity;
            if classes.re_delivered > 0 {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- write-ahead log, by occurrence scan ---------------------------
        {
            let occurrences =
                erasure::count_occurrences(&self.directory.join("wal.log"), &needles)?;
            let mut report = erasure::DomainReport::clear(
                "write-ahead-log",
                match occurrences {
                    0 => "no byte-level occurrence of the subject's identifiers".to_owned(),
                    found => format!(
                        "{found} raw occurrence(s) of the subject's identifiers; \
                         over-approximate — an identifier quoted in unrelated \
                         content counts. A checkpoint rewrites the log; re-run \
                         after one if these are stale frames"
                    ),
                },
            );
            report.occurrences = occurrences;
            if occurrences > 0 {
                report.result = "attention".to_owned();
            }
            domains.push(report);
        }

        // ---- segments -------------------------------------------------------
        {
            let pinned = self.pin_segments()?;
            let mut all_matches: Vec<(String, String, String)> = Vec::new();
            let mut unredacted = 0usize;
            for segment in &pinned {
                if !self.segment_may_hold_subject(segment, subject)? {
                    continue;
                }
                let keys = Self::subject_keys_in_segment(&segment.seg, subject)?;
                unredacted += keys.len();
                all_matches.extend(keys);
            }
            let classes = erasure::classify_matches(&erased_keys, all_matches);
            // One classification rule for every domain: a key the erasure
            // covered, found matching again, is a re-delivery and fails; a
            // fresh key is new activity and is reported. A payload subject
            // is no exception — a NEW span referencing re-uploaded content
            // is post-settle data, and it must not read as `erased` while
            // buffered and `incomplete` the moment a seal moves it into a
            // segment. The buffer above already applies exactly this rule.
            let failing = classes.re_delivered > 0;
            clean &= !failing;
            let mut report = erasure::DomainReport::clear(
                "segments",
                format!(
                    "{} segment(s) checked through their indexes; {}",
                    pinned.len(),
                    match unredacted {
                        0 => "no record matches the subject".to_owned(),
                        found => format!("{found} record(s) match the subject"),
                    }
                ),
            );
            report.re_delivered = classes.re_delivered;
            report.new_activity = classes.new_activity;
            if failing {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- annotation log -------------------------------------------------
        {
            let all = self
                .annotations
                .search(&annotations::AnnotationQuery::default())?;
            // Matched against the annotation's own typed subject, tenant
            // included: a session-subject annotation carries no span address
            // for the key set to catch, and a tenant subject dooms by
            // ownership, not by address.
            let matching = all
                .iter()
                .filter(|annotation| match subject {
                    erasure::Subject::Trace { trace_id, tenant } => {
                        annotation.tenant == *tenant && annotation.trace_id == *trace_id
                    }
                    erasure::Subject::Span {
                        trace_id,
                        span_id,
                        tenant,
                    } => {
                        annotation.tenant == *tenant
                            && annotation.trace_id == *trace_id
                            && annotation.span_id == *span_id
                    }
                    erasure::Subject::Session { session_id, tenant } => {
                        (annotation.tenant == *tenant && annotation.session_id == *session_id)
                            || erased_keys.contains(&(
                                annotation.tenant.clone(),
                                annotation.trace_id.clone(),
                                annotation.span_id.clone(),
                            ))
                    }
                    erasure::Subject::Tenant { tenant } => annotation.tenant == *tenant,
                    erasure::Subject::Payload { .. } => false,
                })
                .count();
            // A tenant subject records no annotation identities, and the
            // tenant may return post-settle. Classify by the erasure's own
            // request time: an annotation stamped before it can only be a
            // survivor (fails); one stamped after is new activity. The
            // timestamp is client-suppliable, so the split is advisory in
            // the failing-safe direction — a forged OLD stamp fails a
            // receipt it should pass, never the reverse.
            let (failing, fresh) = match subject {
                erasure::Subject::Tenant { .. } => {
                    let survivors = all
                        .iter()
                        .filter(|annotation| {
                            matches!(subject, erasure::Subject::Tenant { tenant }
                                if annotation.tenant == *tenant)
                                && annotation.timestamp_ns < record.requested_unix_ns
                        })
                        .count();
                    (survivors, matching - survivors)
                }
                _ => (matching, 0),
            };
            clean &= failing == 0;
            let mut report = erasure::DomainReport::clear(
                "annotations",
                match (failing, fresh) {
                    (0, 0) => "no annotation addresses the subject".to_owned(),
                    (0, fresh) => {
                        format!("{fresh} post-settle annotation(s) under the tenant — new activity")
                    }
                    (found, _) => format!("{found} annotation(s) still address the subject"),
                },
            );
            report.new_activity = fresh;
            if failing > 0 {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- eval records -----------------------------------------------------
        // A decode-walk, never a raw byte scan of the shared log: the walk is
        // scoped to the subject's tenant, so one tenant's receipt can never
        // name another tenant's datasets — and its classifications are exact
        // where a byte scan could only be over-approximate.
        {
            let mut report = match subject {
                erasure::Subject::Tenant { tenant } => {
                    let (re_delivered, new_activity) = self.evals.tenant_record_report(
                        tenant,
                        &record.eval_datasets,
                        &record.eval_experiments,
                    )?;
                    clean &= re_delivered == 0;
                    let mut report = erasure::DomainReport::clear(
                        "eval-records",
                        match (re_delivered, new_activity) {
                            (0, 0) => format!(
                                "no eval record remains for the tenant ({} dataset(s) and \
                                 {} experiment(s) were erased)",
                                record.eval_datasets.len(),
                                record.eval_experiments.len()
                            ),
                            (0, fresh) => format!(
                                "{fresh} eval record(s) under ids allocated after the \
                                 erasure — post-settle new activity; a barrier, not a ban"
                            ),
                            (survivors, _) => format!(
                                "{survivors} eval record(s) still carry ERASED ids — \
                                 the purge did not complete; re-run the erasure"
                            ),
                        },
                    );
                    report.re_delivered = re_delivered;
                    report.new_activity = new_activity;
                    if re_delivered > 0 {
                        report.result = "holds-data".to_owned();
                    }
                    report
                }
                erasure::Subject::Payload { reference } => {
                    let holders = self.evals.references_to(reference)?;
                    let mut report = erasure::DomainReport::clear(
                        "eval-records",
                        match holders.is_empty() {
                            true => "no dataset example references the payload".to_owned(),
                            false => format!(
                                "{} example(s) still carry the reference — an address, \
                                 not content; the bytes are gone and the version's \
                                 digests remain valid. Purging the addresses is a \
                                 dataset-version tombstone plus a future compaction",
                                holders.len()
                            ),
                        },
                    );
                    if !holders.is_empty() {
                        report.result = "retained-by-design".to_owned();
                        report.items = holders;
                    }
                    report
                }
                _ => {
                    let tenant = subject.tenant().unwrap_or("");
                    let copies = self.evals.copies_in_tenant(tenant, &needles)?;
                    let mut report = erasure::DomainReport::clear(
                        "eval-records",
                        match copies.is_empty() {
                            true => {
                                "no dataset example carries the subject's identifiers".to_owned()
                            }
                            false => format!(
                                "{} example(s) carry copies traceable to the subject — \
                                 promotion copies survive source erasure BY DESIGN; \
                                 purging them is a deliberate second act (tombstone the \
                                 version, erase the payload)",
                                copies.len()
                            ),
                        },
                    );
                    if !copies.is_empty() {
                        report.result = "attention".to_owned();
                        report.items = copies;
                    }
                    report
                }
            };
            // Domain order in the receipt is stable; this one sits between
            // annotations and payloads, where its findings read in context.
            if report.detail.is_empty() {
                report.detail = "checked".to_owned();
            }
            domains.push(report);
        }

        // ---- payload files ----------------------------------------------------
        {
            let mut items: Vec<String> = Vec::new();
            let mut failing = false;
            // The references to account for: what the intent record holds
            // (a tenant subject's are noted into it during the purge, so by
            // verify time they are present), PLUS every disposition the
            // settle recorded — without the settle's lists a shared
            // payload's retention would go unnamed in the receipt.
            let mut accountable: Vec<String> = record.payload_refs.clone();
            if let Some(settle) = &status.settle {
                accountable.extend(settle.payloads_removed.iter().cloned());
                accountable.extend(
                    settle
                        .payloads_retained
                        .iter()
                        .map(|retained| retained.reference.clone()),
                );
            }
            accountable.sort();
            accountable.dedup();
            let live = match accountable.is_empty() {
                true => std::collections::HashSet::new(),
                false => self.live_payload_refs()?,
            };
            for reference in &accountable {
                let exists = reference
                    .strip_prefix("sha256/")
                    .map(|hash| payload::payload_path(&self.directory, hash).exists())
                    .unwrap_or(false);
                match (exists, live.contains(reference)) {
                    (false, _) => items.push(format!("{reference}: erased")),
                    (true, true) => items.push(format!(
                        "{reference}: retained — still referenced by live spans or \
                         dataset examples outside the subject (content addressing \
                         shares bytes; reference-aware deletion keeps them)"
                    )),
                    (true, false) => {
                        failing = true;
                        items.push(format!(
                            "{reference}: present and unreferenced — the sweep did \
                             not complete; re-run the erasure"
                        ));
                    }
                }
            }
            clean &= !failing;
            let mut report = erasure::DomainReport::clear(
                "payloads",
                format!("{} reference(s) accounted for", accountable.len()),
            );
            report.items = items;
            if failing {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- derived caches, by occurrence scan ------------------------------
        {
            let mut occurrences = 0usize;
            let pinned = self.pin_segments()?;
            for segment in &pinned {
                let mut sidecar = segment.path.clone().into_os_string();
                sidecar.push(".rollup");
                occurrences += erasure::count_occurrences(Path::new(&sidecar), &needles)?;
            }
            let mut report = erasure::DomainReport::clear(
                "derived-caches",
                match occurrences {
                    0 => "no occurrence of the subject's identifiers in any rollup sidecar"
                        .to_owned(),
                    found => format!(
                        "{found} raw occurrence(s) in rollup sidecars; over-approximate, \
                         and expected exactly where a payload was retained for live \
                         references"
                    ),
                },
            );
            report.occurrences = occurrences;
            if occurrences > 0 {
                report.result = "attention".to_owned();
            }
            domains.push(report);
        }

        // ---- pins ----------------------------------------------------------------
        {
            let labels = erasure::pin_labels(&self.directory)?;
            let mut items: Vec<String> = Vec::new();
            let mut holding = 0usize;
            for label in &labels {
                let pin_dir = self.directory.join(generation::PINS_DIR).join(label);
                // For a payload subject the pinned FILE is the content, and
                // it holds its bytes whether or not any pinned span still
                // references them — hard links share inodes, which is the
                // whole point of a pin and the whole reason to check.
                let mut holds = subject
                    .payload_reference()
                    .and_then(|reference| reference.strip_prefix("sha256/"))
                    .is_some_and(|hash| payload::payload_path(&pin_dir, hash).exists());
                if !holds {
                    for entry in fs::read_dir(&pin_dir)? {
                        let path = entry?.path();
                        if !is_segment_file(&path) {
                            continue;
                        }
                        let seg = segment::Segment::open(&path).map_err(segment_error)?;
                        if !Self::subject_keys_in_segment(&seg, subject)?.is_empty() {
                            holds = true;
                            break;
                        }
                    }
                }
                // A pin copies the manifested prefix of the append-only
                // logs, so a backup taken before the erasure holds the
                // subject's annotations and eval records even when no
                // pinned SEGMENT does — a restore would resurrect them.
                // Both logs are read without healing: a pin is a backup,
                // and verification must never write into one.
                if !holds {
                    let pinned_annotations = pin_dir.join("annotations.jsonl");
                    if pinned_annotations.exists() {
                        let contents = fs::read(&pinned_annotations)?;
                        holds = contents
                            .split(|byte| *byte == b'\n')
                            .filter_map(|line| {
                                serde_json::from_slice::<annotations::Annotation>(line).ok()
                            })
                            .any(|annotation| match subject {
                                erasure::Subject::Trace { trace_id, tenant } => {
                                    annotation.tenant == *tenant && annotation.trace_id == *trace_id
                                }
                                erasure::Subject::Span {
                                    trace_id,
                                    span_id,
                                    tenant,
                                } => {
                                    annotation.tenant == *tenant
                                        && annotation.trace_id == *trace_id
                                        && annotation.span_id == *span_id
                                }
                                erasure::Subject::Session { session_id, tenant } => {
                                    annotation.tenant == *tenant
                                        && annotation.session_id == *session_id
                                }
                                erasure::Subject::Tenant { tenant } => annotation.tenant == *tenant,
                                erasure::Subject::Payload { .. } => false,
                            });
                    }
                }
                if !holds {
                    holds = !evals::pinned_log_findings(
                        &pin_dir.join(evals::LOG_NAME),
                        subject,
                        &needles,
                    )?
                    .is_empty();
                }
                match holds {
                    true => {
                        holding += 1;
                        items.push(format!(
                            "pin {label:?} holds the subject — release it (and re-create \
                             it from the current generation if the backup is still wanted)"
                        ));
                    }
                    false => items.push(format!("pin {label:?}: clear")),
                }
            }
            clean &= holding == 0;
            let mut report = erasure::DomainReport::clear(
                "pins",
                match labels.is_empty() {
                    true => "no pins exist".to_owned(),
                    false => format!("{} pin(s) checked", labels.len()),
                },
            );
            report.items = items;
            if holding > 0 {
                report.result = "holds-data".to_owned();
            }
            domains.push(report);
        }

        // ---- metadata domains, stated rather than implied ----------------------
        domains.push(erasure::DomainReport::clear(
            "generations",
            "manifests carry file paths and digests only; no span content".to_owned(),
        ));
        {
            let mut report = erasure::DomainReport::clear(
                "tombstone-log",
                "retains the subject's identifiers and resolved keys as the record \
                 of this erasure — that record is what this receipt verifies against; \
                 erasing the record of erasure means deleting the store"
                    .to_owned(),
            );
            report.result = "retained-by-design".to_owned();
            domains.push(report);
        }

        // Conclusiveness is computed from the domain reports, never asserted:
        // any over-approximate signal left unexplained — an occurrence scan
        // that found the subject's identifiers, a domain that could only
        // reach "attention" — makes the receipt inconclusive even where the
        // semantic walk is clean. `result` says what the walk found;
        // `conclusive` says whether anything at all was left ambiguous.
        let conclusive = domains
            .iter()
            .all(|domain| domain.occurrences == 0 && domain.result != "attention");
        Ok(erasure::Receipt {
            erasure_id: record.id,
            subject: subject.clone(),
            requested_unix_ns: record.requested_unix_ns,
            verified_unix_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64),
            generation: status.settle.as_ref().map(|settle| settle.generation),
            settled: status.settle.is_some(),
            domains,
            result: match clean {
                true => "erased".to_owned(),
                false => "incomplete".to_owned(),
            },
            conclusive,
        })
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
                log.rewrite(&survivors, 0)?;
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
            .segment_seal_reconcile
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

    fn write_segment<S: std::borrow::Borrow<Span>>(&self, id: u64, spans: &[S]) -> Result<Sealed> {
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
    fn rewrite_segment_in_place(&self, path: &Path, spans: &[Span]) -> Result<Sealed> {
        self.seal_segment(path, spans)
    }

    /// Replaces the cached rollups for `evicted` with `installed`, but ONLY
    /// where something was actually evicted.
    ///
    /// For callers that must INVALIDATE — expiry rewrites a segment in place,
    /// so a cached rollup for that path would count spans TTL just deleted.
    /// A merge wants [`Self::install_cached_rollups`] instead: its inputs stay
    /// valid for anyone still reading them.
    ///
    /// The condition is what keeps this from being a memory regression. A
    /// rollup is not small — eight bytes per span for the supersede prefilter,
    /// plus the per-session trace sets — and installing one for every segment
    /// the store ever writes would make a store that never runs an aggregation
    /// pay for an analytics cache it does not use. Replacing only what was
    /// already resident makes the exchange net-neutral at worst.
    ///
    /// **Callers must have published the new segments first.** Installing a
    /// rollup for a path whose OLD segment is still in the live list would
    /// hand a query the new counters for the old bytes.
    fn replace_cached_rollups(
        &self,
        evicted: &[PathBuf],
        installed: impl IntoIterator<Item = (PathBuf, CachedRollup)>,
    ) {
        let Ok(mut rollups) = self.rollups.lock() else {
            return;
        };
        let mut had_any = false;
        for path in evicted {
            had_any |= rollups.remove(path).is_some();
        }
        if had_any {
            rollups.extend(installed);
        }
    }

    /// Adds `installed` to the cache if any of `warm` is already cached,
    /// leaving `warm` in place.
    ///
    /// The gate is the same one [`Self::replace_cached_rollups`] applies for
    /// the same reason: a store that never runs an aggregation must not
    /// accumulate an analytics cache because it compacted. What differs is
    /// that nothing is evicted — see the merge's publish block for why taking
    /// an input's rollup away mid-fold is the expensive mistake, and
    /// `fold_analytics` for what reclaims them instead.
    ///
    /// **Callers must have published the new segments first**, for the same
    /// reason.
    fn install_cached_rollups(
        &self,
        warm: &[PathBuf],
        installed: impl IntoIterator<Item = (PathBuf, CachedRollup)>,
    ) {
        let Ok(mut rollups) = self.rollups.lock() else {
            return;
        };
        if warm.iter().any(|path| rollups.contains_key(path)) {
            rollups.extend(installed);
        }
    }

    /// Encodes `spans`, fsyncs them into a temp file, and renames that onto
    /// `final_path`. The rename is what makes a segment appear atomically: a
    /// reader sees the whole file or none of it, never a partial one.
    fn seal_segment<S: std::borrow::Borrow<Span>>(
        &self,
        final_path: &Path,
        spans: &[S],
    ) -> Result<Sealed> {
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
            let encoded =
                segment::encode_with(&records, self.config.content_index).map_err(segment_error)?;
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
            let written = Segment {
                path: final_path.to_path_buf(),
                bytes,
                seg,
                key_hashes: std::sync::OnceLock::new(),
            };
            // Write the rollup sidecar now, while the spans are still in hand.
            //
            // Building it here costs one more fold over spans that are already
            // hot in cache; building it later costs a full decode of the
            // segment that was just written. Doing it at seal is also what
            // makes a restart cheap for a store nobody queried before it went
            // down — the lazy path in `Store::segment_rollup` would otherwise
            // only ever heal segments someone had already paid for once.
            //
            // Best-effort by design: this is a derived cache, and a seal that
            // failed because a cache could not be saved would put durability
            // at the mercy of a disposable file. A missing sidecar is simply
            // rebuilt on demand.
            let rollup =
                std::sync::Arc::new(analytics::SegmentRollup::build(spans, self.pricing()));
            let _ = rollup_file::store(
                final_path,
                written.rollup_binding(self.pricing_fingerprint()),
                &rollup,
            );
            // Handed back rather than dropped: the caller may be replacing a
            // segment whose rollup was cached, and rebuilding what it is about
            // to throw away — from a file that was just written from the very
            // spans still in scope — is work nobody needs to do twice.
            Ok(Sealed {
                segment: written,
                rollup,
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

/// Length of the maximal same-tier run at the TAIL of `segments`, when
/// merging that run would leave fewer segments than it consumed.
///
/// Tail-only is a correctness requirement, not a simplification: see
/// [`Store::compact_segments`].
///
/// **Only a LARGER segment ends the run.** The tier that matters is the
/// largest in the run, not the tier of the last segment, and a segment
/// smaller than that rides along as a passenger. Anchoring on the last
/// segment and stopping at any tier change made every tier discontinuity
/// permanent, in both directions. Ingest flushes a partial segment whenever a
/// batch does not divide evenly into `flush_spans`, and a partial below
/// `base_bytes` is tier 0 among tier-1 neighbours: at the tail it made the
/// run length 1, below `fanout`, so nothing merged; once ingest moved past it
/// it became a wall in the middle, and since nothing behind the tail is ever
/// a candidate again, everything older than it was frozen for good.
/// Absorbing a smaller neighbour costs almost nothing — it is a rounding
/// error against the run it joins — whereas stopping at one costs the whole
/// prefix.
fn tail_run_to_merge(
    segments: &[std::sync::Arc<Segment>],
    settings: &CompactionConfig,
) -> Option<usize> {
    if segments.len() < settings.fanout {
        return None;
    }
    let mut tier = size_tier(segments.last()?.bytes, settings);
    let mut run = 0usize;
    // Only segments AT the anchor tier count toward `fanout`; passengers are
    // in the run but do not justify it. Merging four tiny segments into a
    // 256 MiB one is the write amplification the tiers exist to prevent, so
    // a passenger must never be what makes a run look long enough.
    let mut counted = 0usize;
    for segment in segments.iter().rev() {
        let segment_tier = size_tier(segment.bytes, settings);
        if segment_tier > tier {
            if counted >= settings.fanout {
                // A run worth merging already, and the bigger segment behind
                // it is exactly what should not be rewritten to absorb it.
                break;
            }
            // Otherwise it becomes the anchor: the smaller ones picked up so
            // far were the passengers, and this is the real run.
            tier = segment_tier;
            counted = 0;
        }
        run += 1;
        if segment_tier == tier {
            counted += 1;
        }
    }
    if counted < settings.fanout {
        return None;
    }
    // The size cap bounds each OUTPUT, not the run, because the merge emits
    // as many segments as it needs (see `merge_chunks`). What the run must
    // still clear is that merging it is worth doing at all: a run already
    // made of cap-sized segments would be rewritten byte for byte to produce
    // exactly as many segments as it consumed.
    let start = segments.len() - run;
    if merge_chunks(&segments[start..], settings).len() >= run {
        return None;
    }
    Some(run)
}

/// Splits a merge's inputs into the consecutive groups that each become one
/// output segment, so no output exceeds `max_segment_bytes`.
///
/// Grouping rather than one big merge is what lets compaction work down a
/// backlog. A capped merge used to stop partway along the run and leave a
/// cap-sized segment at the tail, which — being a different tier from the
/// smaller segments behind it — blocked them permanently, so a store that
/// accumulated segments faster than it compacted them could never catch up.
///
/// Groups are consecutive and their outputs take ascending ids in the same
/// order, which is what keeps last-write-wins intact: a key present in two
/// groups lands in two outputs, and the later output — holding the version
/// from the later input — sorts after the earlier one, exactly as its source
/// segments did. That is why dedup can stay within a group, and why a merge
/// only ever needs one group's spans in memory at a time.
fn merge_chunks(run: &[std::sync::Arc<Segment>], settings: &CompactionConfig) -> Vec<usize> {
    let mut chunks = Vec::new();
    let mut current = 0usize;
    let mut total = 0u64;
    for segment in run {
        let projected = total.saturating_add(segment.bytes);
        // A segment larger than the cap on its own still forms a group: the
        // cap cannot be honoured for it either way, and refusing would stall.
        if current > 0 && settings.max_segment_bytes > 0 && projected > settings.max_segment_bytes {
            chunks.push(current);
            current = 1;
            total = segment.bytes;
        } else {
            current += 1;
            total = projected;
        }
    }
    if current > 0 {
        chunks.push(current);
    }
    chunks
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

/// Compaction journal: one record per merge, naming the segments it consumes
/// and the segments that replace them. Written BEFORE any replacement exists,
/// deleted after the originals are removed, so recovery can finish an
/// interrupted merge in either direction without ever guessing from content —
/// content-based duplicate healing silently destroyed legitimately re-ingested
/// identical spans (found in review: acknowledged duplicate cardinality must
/// survive restart).
fn merge_journal_path(directory: &Path, first_output: &str) -> PathBuf {
    directory.join(format!(".supersede.{first_output}.journal"))
}

/// Records a merge as one transaction: every input it consumes and every
/// output that together supersede them.
///
/// Named for its first output, whose id is claimed under the segment lock and
/// so is unique and ascending — which makes path order merge order when
/// recovery has more than one to finish.
fn write_merge_journal(directory: &Path, inputs: &[String], outputs: &[String]) -> Result<PathBuf> {
    let path = merge_journal_path(directory, outputs.first().map_or("", String::as_str));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(&path)?;
    writeln!(file, "inputs {}", inputs.join(","))?;
    writeln!(file, "outputs {}", outputs.join(","))?;
    file.sync_all()?;
    sync_directory(directory)?;
    Ok(path)
}

/// Finishes one interrupted merge, given its journal's contents.
///
/// **The decision is about the group, never a single file.** A merge deletes
/// its inputs only once every output is durable, so "every input is still
/// here" is what proves it never committed — and that is the only state in
/// which undoing it is right. Any input already gone proves the opposite:
/// deletion had started, so every output was durable at that moment, and an
/// output missing now is simply one a LATER merge has since consumed, with
/// that merge's own output carrying the data forward. Undoing then would
/// delete live segments holding the only copy of an input already removed.
fn recover_merge(directory: &Path, journal: &str) -> Result<()> {
    let field = |key: &str| -> Vec<&str> {
        journal
            .lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(|rest| rest.split(',').filter(|name| !name.is_empty()).collect())
            .unwrap_or_default()
    };
    let inputs = field("inputs ");
    let outputs = field("outputs ");
    if outputs.is_empty() {
        // Nothing this journal describes can exist yet. Either a crash tore
        // it mid-fsync — it is written before the first output — or it was
        // left by a version that journaled one input at a time, whose merges
        // produced a single output renamed atomically, so the store is
        // consistent whichever side that crash landed on.
        return Ok(());
    }
    let ready = |name: &str| {
        let path = directory.join(name);
        path.exists() && (!name.ends_with(SEGMENT_SUFFIX) || segment::Segment::open(&path).is_ok())
    };
    let committed = outputs.iter().all(|name| ready(name));
    let untouched = inputs.iter().all(|name| directory.join(name).exists());
    if committed || !untouched {
        // Forward: the outputs stand and the inputs are dead. Idempotent, so
        // a second crash mid-deletion simply resumes here.
        for name in &inputs {
            unlink_segment(&directory.join(name))?;
        }
    } else {
        // Back: no output may outlive a merge that never committed.
        for name in &outputs {
            unlink_segment(&directory.join(name))?;
        }
    }
    sync_directory(directory)
}

/// Finishes interrupted merges recorded in the compaction journal.
///
/// Each journal is one merge, resolved as a unit by [`recover_merge`] and then
/// removed — the removal last and synced, so a journal never disappears before
/// the deletions it authorized are durable. Journals are taken in path order,
/// which is merge order: each is named for its first output, and ids ascend.
fn recover_supersede_markers(directory: &Path) -> Result<()> {
    let mut journals = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".supersede.") && name.ends_with(".journal") {
            journals.push(entry.path());
        }
    }
    journals.sort();
    for journal in journals {
        recover_merge(directory, &fs::read_to_string(&journal).unwrap_or_default())?;
        fs::remove_file(&journal)?;
        sync_directory(directory)?;
    }
    Ok(())
}

// Traza's stable span order: (start, end, tenant, trace, span). The tenant
// sits before the ids so the order is total over the full primary key; for a
// single-tenant store every tenant is "" and the order is exactly what it was
// before tenancy existed.
fn compare_spans(left: &Span, right: &Span) -> std::cmp::Ordering {
    left.start_time_ns
        .cmp(&right.start_time_ns)
        .then_with(|| left.end_time_ns.cmp(&right.end_time_ns))
        .then_with(|| left.tenant.cmp(&right.tenant))
        .then_with(|| left.trace_id.cmp(&right.trace_id))
        .then_with(|| left.span_id.cmp(&right.span_id))
}

fn compare_span_cursor(span: &Span, cursor: &SpanCursor) -> std::cmp::Ordering {
    span.start_time_ns
        .cmp(&cursor.start_time_ns)
        .then_with(|| span.end_time_ns.cmp(&cursor.end_time_ns))
        .then_with(|| span.tenant.cmp(&cursor.tenant))
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
/// counted, not estimated — the smallest one is genuinely the least work.
/// (A list is a digest-keyed CANDIDATE set, so a length is an upper bound
/// rather than a match count. The gap is a collision away from zero, and every
/// candidate is checked by `span_matches` regardless.)
fn select_probe<'a>(
    seg: &'a segment::Segment,
    filter: &SpanFilter,
    content: Option<&content::Query>,
    metrics: &metrics::Metrics,
) -> Result<Cow<'a, [u64]>> {
    // The chosen list, and whether the content index is what produced it.
    // The flag exists so the admission metric counts records the query will
    // actually decode: incrementing when the content list was merely
    // CONSIDERED overcounted every query where a narrower attribute probe
    // won, and those records are never read.
    let mut best: Option<(Cow<'a, [u64]>, bool)> = None;
    let mut consider = |offsets: Cow<'a, [u64]>, from_content: bool| {
        // `Option::is_none_or` would read better but is newer than this
        // crate's MSRV.
        if best
            .as_ref()
            .map_or(true, |(current, _)| offsets.len() < current.len())
        {
            best = Some((offsets, from_content));
        }
    };
    if let Some(service) = &filter.service {
        consider(
            Cow::Borrowed(seg.attribute_candidate_offsets(IDX_SERVICE, service)),
            false,
        );
    }
    if let Some(name) = &filter.name {
        consider(
            Cow::Borrowed(seg.attribute_candidate_offsets(IDX_NAME, name)),
            false,
        );
    }
    // A non-empty tenant scope probes the reserved tenant posting. An
    // explicit default-tenant scope (`Some("")`) cannot: empty tenants are
    // deliberately never indexed (byte-identity for single-tenant stores),
    // so that filter falls through to whatever other probe the query has —
    // the tenant clause in `span_matches_without_content` still decides.
    if let Some(tenant) = filter.tenant.as_deref() {
        if !tenant.is_empty() {
            consider(
                Cow::Borrowed(seg.attribute_candidate_offsets(IDX_TENANT, tenant)),
                false,
            );
        }
    }
    for (key, value) in &filter.attributes {
        // Session keys are expanded by the caller into a union and cannot
        // drive a single probe.
        if key.starts_with('\u{0}') {
            continue;
        }
        consider(attribute_candidates(seg, key, value), false);
    }
    // The content index is just another candidate source, and often the most
    // selective one: an attribute probe narrows to a value, a content probe
    // narrows to the 128-record blocks that may hold a word. It costs a few
    // small reads, so it is consulted only when the filter asks for it.
    if let Some(query) = content {
        if let Some(offsets) = seg
            .content_candidate_offsets(query)
            .map_err(segment_error)?
        {
            consider(Cow::Owned(offsets), true);
        }
    }
    // Nothing indexable: every record is a candidate.
    let Some((offsets, from_content)) = best else {
        return Ok(Cow::Borrowed(seg.record_offsets()));
    };
    if from_content {
        metrics
            .records_admitted_by_content
            .add(offsets.len() as u64);
    }
    Ok(offsets)
}

/// Whether `span` satisfies `filter`.
///
/// `content` is the filter's content query, parsed once by the caller rather
/// than per span — tokenizing the needle for every candidate would cost more
/// than the index saves. It is a separate parameter rather than a lookup
/// inside `filter` so that every verification site has to acknowledge it: a
/// path that forgot to check content would return spans that do not match,
/// and the content index's whole safety argument is that the decoded span
/// decides.
fn span_matches(span: &Span, filter: &SpanFilter, content: Option<&content::Query>) -> bool {
    if let Some(query) = content {
        if !query.matches(content_strs(span).into_iter()) {
            return false;
        }
    }
    span_matches_without_content(span, filter)
}

fn span_matches_without_content(span: &Span, filter: &SpanFilter) -> bool {
    // The tenant clause is first because it is the isolation boundary: every
    // scoped surface relies on this one comparison, and an index probe that
    // over-selected across tenants is corrected here (invariant 7).
    if filter
        .tenant
        .as_ref()
        .is_some_and(|tenant| span.tenant != *tenant)
    {
        return false;
    }
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
        .status
        .as_ref()
        .is_some_and(|status| span.status != *status)
    {
        return false;
    }
    if filter.excluded_statuses.contains(&span.status) {
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
            // Legacy JSONL segments (on-disk format 1) are not readable: failing loudly
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
        let seg = Box::new(segment::Segment::open(&path).map_err(|error| {
            // Name the file, and give advice only where the cause is actually
            // known. An earlier version attached "remove the data directory" to
            // every `Unsupported`, which covers a corrupt magic byte as well as
            // a version mismatch — so a single flipped bit in an otherwise
            // intact store was answered with instructions to delete it.
            //
            // Nothing here ever recommends deleting data, and nothing claims
            // more than the check established. A version mismatch is detected
            // before any section bound is validated, so it does NOT prove the
            // file is otherwise intact — it says only that another build reads
            // this layout. The path it points at is a backup taken the way
            // docs/operations/durability.md requires, not an export: an export
            // carries spans (buffered ones included, since it pins a snapshot
            // and the snapshot copies the write buffer) but leaves payload
            // bytes and annotations behind.
            let detail = match &error {
                segment::Error::UnsupportedVersion { .. } => format!(
                    "\nBack up the directory first — stop the server and copy it, \
                     or take a filesystem snapshot atomic across the whole \
                     directory. A file-by-file copy of a running store is not \
                     safe.\n\
                     A store can hold segments in SEVERAL formats — each build \
                     writes the format of its day and leaves earlier segments \
                     alone — so a reader has to cover all of them, not just \
                     this one. Commit {LEGACY_SEGMENT_READER} reads formats 2 \
                     through 5; build it and open the backup with that.\n\
                     A span export carries every span, buffered ones included, \
                     but offloaded values stay as $payload references and \
                     annotations are not in it at all.\n\
                     See docs/operations/durability.md#backups and \
                     docs/segment-format.md."
                ),
                segment::Error::Unsupported(_) | segment::Error::Corrupt(_) => {
                    // Deliberately does NOT say another build can read the rest:
                    // `load_segments` aborts on the first unreadable segment, so
                    // no build opens this store until the file is dealt with.
                    "\nThis file is not a segment this build can interpret. It \
                     may be truncated, damaged, or not a Traza file at all. No \
                     build will open this store until it is resolved, so copy \
                     the directory and inspect the file before changing \
                     anything."
                        .to_owned()
                }
                _ => String::new(),
            };
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}{detail}", path.display()),
            ))
        })?);
        segments.push(std::sync::Arc::new(Segment {
            path,
            bytes: bytes_meta,
            seg,
            key_hashes: std::sync::OnceLock::new(),
        }));
    }
    Ok(segments)
}

/// Deletes `.rollup` sidecars with no matching segment in `segments`.
fn remove_orphan_rollups(directory: &Path, segments: &[std::sync::Arc<Segment>]) -> Result<()> {
    let live: std::collections::HashSet<PathBuf> = segments
        .iter()
        .map(|segment| rollup_file::rollup_path(&segment.path))
        .collect();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let is_rollup = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(SEGMENT_PREFIX) && name.ends_with(".rollup"));
        if is_rollup && !live.contains(&path) {
            // Best-effort: an undeletable stale sidecar is wasted space, not
            // a reason to refuse to open the store.
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
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
    // The rollup sidecar dies with its segment. It goes FIRST: a sidecar
    // without a segment is a file nothing will ever look at again, whereas a
    // segment without its sidecar is the ordinary state of every store that
    // has not been queried yet. Failing to remove it is not worth failing the
    // unlink over — the binding check makes a stale sidecar unusable anyway,
    // and `remove_orphan_rollups` sweeps it on the next open.
    let _ = rollup_file::remove(path);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn segment_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    // Both suffixes count: recognizing only .jsonl made a reopened indexed-only
    // store restart numbering at zero, and the next flush RENAMED OVER
    // segment-…0000.seg — persisted spans destroyed (found in review,
    // reproduced across restart).
    let stem = name.strip_prefix(SEGMENT_PREFIX)?;
    let number = stem
        .strip_suffix(LEGACY_SEGMENT_SUFFIX)
        .or_else(|| stem.strip_suffix(SEGMENT_SUFFIX))?;
    number.parse().ok()
}

/// Adopts a directory into the generation layout by publishing generation one
/// over the files already in it. Returns the live generation id.
///
/// The engine files never move — the layout keeps them at the root — so this
/// only adds metadata: it converts a pre-generation log to stamped framing,
/// digests the working set, writes generation one's manifest, and publishes
/// `CURRENT`. One-way and resumable: this runs only while `CURRENT` is absent,
/// each step is idempotent (the log conversion recognizes a log already
/// converted), and the commit is the `CURRENT` rename, so a crash before it
/// leaves a directory the next open re-adopts and a crash after it leaves an
/// adopted store. A brand-new empty directory takes the same path and gets an
/// empty generation one.
///
/// The pre-generation log's spans are replayed with the old reader and
/// rewritten as ONE stamped frame under `(epoch 1, sequence 1)`. Replay order
/// is preserved inside the frame, so last-write-wins resolves exactly as it
/// did, and generation one's `folded_through` of zero leaves the frame
/// strictly after it — the spans replay into the buffer on the very open that
/// adopts them.
fn migrate_to_generations(root: &Path) -> Result<u64> {
    // Convert the log, unless a resumed migration already did.
    let wal_path = root.join("wal.log");
    let already_v2 = match File::open(&wal_path) {
        Ok(mut file) => {
            let mut magic = [0u8; 8];
            let mut filled = 0;
            while filled < magic.len() {
                match io::Read::read(&mut file, &mut magic[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(Error::Io(error)),
                }
            }
            filled == magic.len() && &magic == b"TRZWAL02"
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(Error::Io(error)),
    };
    if !already_v2 {
        let mut replayed: Vec<Span> = Vec::new();
        wal::Wal::recover_v1(root, |span| replayed.push(span))?;
        if !replayed.is_empty() || wal_path.exists() {
            wal::Wal::write_fresh(root, &replayed, 1)?;
        }
    }

    // Generation one: everything the working set holds, nothing folded — the
    // converted frame replays. The manifest is durable before CURRENT names
    // it, and CURRENT is the commit.
    let files = generation::digest_engine(root, &[])?;
    let created_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
    generation::write_manifest(
        root,
        &generation::Manifest {
            generation: 1,
            created_unix_ns,
            folded_through: generation::FoldedThrough::NONE,
            files,
        },
    )?;
    generation::publish_current(root, 1)?;
    Ok(1)
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
        assert!(!buffer.contains_key("", "t", "a"));
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
        assert!(buffer.contains_key("", "t", "a"));
        assert!(!buffer.contains_key("", "t", "b"));
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
    fn compaction_waits_for_a_seal_rather_than_claiming_an_id_beside_it() {
        // A seal that claimed a LOWER id but has not published it yet would
        // sort BEFORE a merge that claims its id now — and the merge's outputs
        // are strictly older data, so they would win. The seal permit is what
        // rules that out, and compaction WAITS for it. Declining instead was
        // correct and useless: a seal is in flight for much of the time under
        // load, so compaction simply stopped running.
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
        let store = std::sync::Arc::new(Store::open(&dir, config).expect("open"));
        for index in 0..4 {
            store
                .ingest(span("t", &format!("s{index}"), "v1"))
                .expect("ingest");
            store.flush().expect("flush");
        }
        assert_eq!(store.stats().expect("stats").segment_count, 4);

        // Stand in for a seal between its drain and its publish.
        let permit = store.sealing.lock().expect("permit");
        let compactor = {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || store.compact_segments().expect("compact"))
        };
        // It must not have published anything while the permit is out. Not a
        // proof on its own — the thread may simply not have run yet — but it
        // fails loudly if compaction ever stops taking the permit at all.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            store.stats().expect("stats").segment_count,
            4,
            "no merge may publish while a seal holds an unpublished id"
        );

        drop(permit);
        assert!(
            compactor.join().expect("compactor") > 0,
            "and it must proceed once the permit is free, not give up"
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
                    content: Vec::new(),
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
            select_probe(&seg, &service_only, None, &metrics::Metrics::default())
                .expect("plan")
                .len(),
            100,
            "service alone can only probe the whole segment"
        );

        let both = SpanFilter {
            service: Some("svc".to_owned()),
            attributes: vec![("rare".to_owned(), Value::String("yes".to_owned()))],
            ..SpanFilter::default()
        };
        assert_eq!(
            select_probe(&seg, &both, None, &metrics::Metrics::default())
                .expect("plan")
                .len(),
            1,
            "with a selective attribute available, the scan must follow it"
        );

        let no_predicate = SpanFilter::default();
        assert_eq!(
            select_probe(&seg, &no_predicate, None, &metrics::Metrics::default())
                .expect("plan")
                .len(),
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
        assert!(
            select_probe(&seg, &absent, None, &metrics::Metrics::default())
                .expect("plan")
                .is_empty()
        );
    }
}
