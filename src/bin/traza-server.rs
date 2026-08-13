//! HTTP server binary for Traza, backed by the Traza engine.
//!
//! The engine is the single authoritative datastore: every ingest goes through
//! [`traza::Store`] and every read comes back out of it. The server owns
//! no span storage of its own — no side log, no in-memory index — so anything
//! it accepts is durable under the engine's flush/segment rules and visible to
//! any other engine reader of the same directory once flushed.
//!
//! Wire contract (unchanged from the log-backed server):
//! - `POST /v1/spans` with a JSON array of spans or `{"spans": [...]}`;
//!   responds `{"accepted": N, "durability": "buffered|wal|flushed"}`,
//!   naming what the acknowledgement guarantees.
//! - `GET /v1/traces/<trace_id>` responds `{"trace_id": .., "spans": [..]}`
//!   or 404 `{"error": "trace not found"}`.
//! - `GET /v1/spans?service=&name=&content=&min_duration_ns=&since_ns=&until_ns=&limit=`
//!   responds with the matching spans ordered by start time.
//! - `GET /v1/stats` responds with engine statistics.
//! - `GET /v1/sessions?since=&until=&limit=` lists sessions (spans carrying a
//!   recognized session key: `session.id`, `gen_ai.conversation.id`, or a
//!   `traceloop.association.properties.*` key), most recent activity first.
//! - `GET /v1/sessions/<id>` responds with the session rollup and its
//!   per-trace breakdown, or 404.
//! - `GET /v1/stats/llm?group_by=model|provider|service|session|day&since=&until=`
//!   responds with token/cost aggregation rows.
//! - `POST /v1/flush` forces buffered spans into a durable segment.
//! - `POST /v1/traces` accepts OTLP/HTTP JSON or binary protobuf.
//! - `GET /v1/export` streams chunked NDJSON with completion/count trailers.
//! - `GET /v1/tail` streams spans as server-sent events, in ADMISSION order —
//!   the one surface not ordered by event time, because "as they land" cannot
//!   be expressed as a `start_time_ns` window (see [`traza::tail`]).
//!   Both stream routes always close their connection: neither has a declared
//!   length, and neither ends until the consumer does.
//! - `GET /v1/metrics` reports per-stage ingest timings and request counters
//!   in Prometheus text format.
//!
//! **Connections persist.** HTTP/1.1 keep-alive is the default (HTTP/1.0 needs
//! to ask), which makes request framing security-relevant: anything ambiguous
//! about where a body ends would let one request be split into two, the second
//! attributed to the client's next request. So transfer-encoded bodies and
//! duplicate `Content-Length` headers are refused rather than resolved, and
//! any response sent without reading the request's body closes the connection.
//! Concurrency is bounded by CONNECTIONS, not by a queue: a persistent
//! connection occupies its handler until the client is done with it, so
//! queueing past the limit would leave clients waiting indefinitely instead of
//! being told the server is full.
//! - `GET /` and `GET /dashboard` serve the built dashboard. It is read from
//!   disk, never compiled in, so building the server needs no Node toolchain
//!   and a rebuilt UI is picked up without restarting. With no `--ui-dir` the
//!   server searches `$TRAZA_UI_DIR`, `<binary dir>/ui`,
//!   `<binary dir>/../share/traza/ui`, then `./ui/dist` — so a packaged
//!   install works by dropping the build beside the executable. The shell is
//!   served before the auth gate — it carries no data, and its `/v1` calls
//!   stay gated — and the routes 404, listing every path searched, when no
//!   build is found.

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use traza::tail::{TailCursor, TailRead};
use traza::{CompactionConfig, Config, Durability, Profile, Span, SpanCursor, SpanFilter, Store};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
// Headers get a far tighter budget than bodies: without one, a 64 MiB
// garbage "header" plus a 64 MiB declared body doubles the documented
// per-request memory ceiling.
const MAX_HEADER_BYTES: usize = 64 * 1024;
// Per-read/write socket deadline. A connection that goes silent mid-request
// (or never sends one) must release its worker thread instead of parking it
// forever. TRAZA_SOCKET_TIMEOUT_MS overrides (primarily for tests).
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

fn socket_timeout() -> Duration {
    std::env::var("TRAZA_SOCKET_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(SOCKET_TIMEOUT)
}
// A persistent connection is recycled rather than trusted indefinitely: this
// bounds how long one client can hold a thread without reconnecting, and keeps
// any per-connection state from growing without limit.
const MAX_REQUESTS_PER_CONNECTION: usize = 100_000;
// Concurrent connections. Keep-alive means a connection occupies its handler
// for as long as the client keeps it open, so this — not a request queue — is
// what bounds server concurrency. Past it, clients are refused immediately
// with 503 instead of being silently queued behind long-lived connections.
const DEFAULT_MAX_CONNECTIONS: usize = 1_024;
// Concurrent in-flight 503 refusals. See the refusal path in `run`.
const MAX_REFUSAL_THREADS: usize = 64;
// How long a refusal will spend making itself deliverable before giving up.
const REFUSAL_DEADLINE: Duration = Duration::from_millis(250);

/// Tells a client the server is at its connection limit, and makes sure it can
/// actually read that.
///
/// A plain write-then-close loses the response whenever the client's request
/// is still sitting unread in the socket: `close(2)` with unread input sends
/// RST, and the RST beats the buffered 503 to the client. Draining first turns
/// that into an ordinary close.
fn refuse_connection(mut stream: TcpStream) {
    let body = b"{\"error\":\"server at connection limit\"}";
    let _ = stream.set_write_timeout(Some(REFUSAL_DEADLINE));
    let _ = stream.set_read_timeout(Some(REFUSAL_DEADLINE));
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.write_all(body).is_err() {
        return;
    }
    let _ = stream.flush();
    // FIN now, so the client sees the end of the response even while we are
    // still reading what it had already sent.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let deadline = std::time::Instant::now() + REFUSAL_DEADLINE;
    let mut sink = [0_u8; 4096];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

/// Which kind of work a request was, for latency attribution.
///
/// One global request histogram could not answer "how fast are queries": an
/// ingest batch and a trace lookup differ by orders of magnitude, and blending
/// them produced a number that described neither. The classes are coarse on
/// purpose — per-path histograms would grow without bound as routes are added,
/// and nobody tunes at that resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteClass {
    /// Span ingest, native or OTLP.
    Ingest,
    /// A trace or session fetched by id.
    Lookup,
    /// Filtered span search.
    Search,
    /// Aggregation: analytics, series, duration, failures.
    Stats,
    /// Responses held open for as long as the consumer wants them: the live
    /// tail and export. Counted, never timed — see [`ServerMetrics::observe`].
    Stream,
    /// The MCP endpoint. Its own class because one `POST /v1/mcp` can be a
    /// lookup, a search or a whole-store rollup depending only on the tool
    /// named in the body — blending that into `other` alongside static assets
    /// would describe neither.
    Mcp,
    /// Everything else — dashboard assets, metrics, flush.
    Other,
}

impl RouteClass {
    /// Classifies a request by method and path.
    fn of(method: &str, path: &str) -> Self {
        match (method, path) {
            (_, traza::mcp::ENDPOINT) => Self::Mcp,
            ("POST", "/v1/spans" | "/v1/traces") => Self::Ingest,
            ("GET", "/v1/spans") => Self::Search,
            // Long-lived responses, held open by the consumer rather than by
            // the server's own work. Export moved out of `search` with the
            // tail: its duration tracks dataset size, so it was already
            // dragging the search percentiles toward whatever the largest
            // export happened to be.
            ("GET", "/v1/export" | "/v1/tail") => Self::Stream,
            ("GET", path) if path.starts_with("/v1/stats") => Self::Stats,
            ("GET", "/v1/sessions") => Self::Stats,
            ("GET", path)
                if path.starts_with("/v1/traces/")
                    || path.starts_with("/v1/sessions/")
                    || path.starts_with("/v1/payloads/") =>
            {
                Self::Lookup
            }
            ("GET", "/v1/annotations") => Self::Lookup,
            _ => Self::Other,
        }
    }

    /// The metric-name infix for this class.
    fn label(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Lookup => "lookup",
            Self::Search => "search",
            Self::Stats => "stats",
            Self::Stream => "stream",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }
}

/// Server-side request instrumentation, alongside the engine's own.
#[derive(Debug)]
struct ServerMetrics {
    /// When the process began serving, for uptime.
    started: std::time::Instant,
    /// Requests served to completion.
    requests: traza::metrics::Counter,
    /// End-to-end handling time, from parsed head to written response.
    request_latency: traza::metrics::Latency,
    /// The same, split by [`RouteClass`], in that enum's declaration order.
    ///
    /// Must stay the same length as [`ServerMetrics::CLASSES`]. The reporting
    /// zips the two, so a shorter array here does not fail to compile — it
    /// silently drops the classes past the end.
    by_class: [traza::metrics::Latency; Self::CLASSES.len()],
    /// Responses issued per status class, indexed `2xx, 4xx, 5xx`.
    status_2xx: traza::metrics::Counter,
    status_4xx: traza::metrics::Counter,
    status_5xx: traza::metrics::Counter,
    /// Requests refused by the auth gate.
    rejected: traza::metrics::Counter,
    /// Turning a request body into spans: JSON decode, or protobuf decode plus
    /// the OTLP mapping. Separated from the engine stages so "the wire format
    /// is the bottleneck" is a measurement rather than an assumption.
    decode: traza::metrics::Latency,
    /// Spans decoded, so decode cost per span is derivable.
    decoded_spans: traza::metrics::Counter,
    /// Connections refused because the server was at its connection limit.
    /// The backpressure signal: nonzero means clients were shed, and any
    /// throughput number measured during that window is suspect.
    connections_refused: traza::metrics::Counter,
    /// Connections accepted.
    connections_accepted: traza::metrics::Counter,
    /// Connections currently being served.
    connections_live: AtomicUsize,
}

// `Instant` has no `Default`, and the right value is "when this was built" —
// which is process start, since the server makes exactly one of these.
impl Default for ServerMetrics {
    fn default() -> Self {
        Self {
            started: std::time::Instant::now(),
            requests: traza::metrics::Counter::default(),
            request_latency: traza::metrics::Latency::default(),
            by_class: std::array::from_fn(|_| traza::metrics::Latency::default()),
            status_2xx: traza::metrics::Counter::default(),
            status_4xx: traza::metrics::Counter::default(),
            status_5xx: traza::metrics::Counter::default(),
            rejected: traza::metrics::Counter::default(),
            decode: traza::metrics::Latency::default(),
            decoded_spans: traza::metrics::Counter::default(),
            connections_refused: traza::metrics::Counter::default(),
            connections_accepted: traza::metrics::Counter::default(),
            connections_live: AtomicUsize::new(0),
        }
    }
}

impl ServerMetrics {
    fn render_prometheus(&self, into: &mut String) {
        use std::fmt::Write as _;
        let _ = writeln!(into, "# TYPE traza_uptime_seconds gauge");
        let _ = writeln!(
            into,
            "traza_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );
        let _ = writeln!(into, "# TYPE traza_http_requests_total counter");
        let _ = writeln!(into, "traza_http_requests_total {}", self.requests.get());
        let _ = writeln!(into, "# TYPE traza_http_rejected_total counter");
        let _ = writeln!(into, "traza_http_rejected_total {}", self.rejected.get());
        let _ = writeln!(into, "# TYPE traza_http_connections_refused_total counter");
        let _ = writeln!(
            into,
            "traza_http_connections_refused_total {}",
            self.connections_refused.get()
        );
        let _ = writeln!(into, "# TYPE traza_http_connections_accepted_total counter");
        let _ = writeln!(
            into,
            "traza_http_connections_accepted_total {}",
            self.connections_accepted.get()
        );
        let _ = writeln!(into, "# TYPE traza_http_connections_live gauge");
        let _ = writeln!(
            into,
            "traza_http_connections_live {}",
            self.connections_live.load(Ordering::Relaxed)
        );
        let _ = writeln!(into, "# TYPE traza_http_decoded_spans_total counter");
        let _ = writeln!(
            into,
            "traza_http_decoded_spans_total {}",
            self.decoded_spans.get()
        );
        let _ = writeln!(into, "# TYPE traza_http_decode_ns_count counter");
        let _ = writeln!(into, "traza_http_decode_ns_count {}", self.decode.count());
        let _ = writeln!(into, "# TYPE traza_http_decode_ns_sum counter");
        let _ = writeln!(into, "traza_http_decode_ns_sum {}", self.decode.total_ns());
        let _ = writeln!(into, "# TYPE traza_http_decode_ns_max gauge");
        let _ = writeln!(into, "traza_http_decode_ns_max {}", self.decode.max_ns());
        let _ = writeln!(into, "# TYPE traza_http_request_ns_count counter");
        let _ = writeln!(
            into,
            "traza_http_request_ns_count {}",
            self.request_latency.count()
        );
        let _ = writeln!(into, "# TYPE traza_http_request_ns_sum counter");
        let _ = writeln!(
            into,
            "traza_http_request_ns_sum {}",
            self.request_latency.total_ns()
        );
        let _ = writeln!(into, "# TYPE traza_http_request_ns_max gauge");
        let _ = writeln!(
            into,
            "traza_http_request_ns_max {}",
            self.request_latency.max_ns()
        );
        // Percentiles are now bucket upper bounds within 6.25%, which is
        // close enough to publish — the coarse power-of-two bucketing that
        // made the old guidance "do not publish these" is gone.
        for percentile in [50.0, 95.0, 99.0] {
            let _ = writeln!(into, "# TYPE traza_http_request_ns_p{percentile:.0} gauge");
            let _ = writeln!(
                into,
                "traza_http_request_ns_p{percentile:.0} {}",
                self.request_latency.percentile_ns(percentile)
            );
        }
        for (class, latency) in Self::CLASSES.iter().zip(self.by_class.iter()) {
            let label = class.label();
            let _ = writeln!(into, "# TYPE traza_http_{label}_ns_count counter");
            let _ = writeln!(into, "traza_http_{label}_ns_count {}", latency.count());
            let _ = writeln!(into, "# TYPE traza_http_{label}_ns_sum counter");
            let _ = writeln!(into, "traza_http_{label}_ns_sum {}", latency.total_ns());
            for percentile in [50.0, 95.0, 99.0] {
                let _ = writeln!(into, "# TYPE traza_http_{label}_ns_p{percentile:.0} gauge");
                let _ = writeln!(
                    into,
                    "traza_http_{label}_ns_p{percentile:.0} {}",
                    latency.percentile_ns(percentile)
                );
            }
        }
        for (name, counter) in [
            ("traza_http_responses_2xx_total", &self.status_2xx),
            ("traza_http_responses_4xx_total", &self.status_4xx),
            ("traza_http_responses_5xx_total", &self.status_5xx),
        ] {
            let _ = writeln!(into, "# TYPE {name} counter");
            let _ = writeln!(into, "{name} {}", counter.get());
        }
    }

    /// The route classes, in the order [`Self::by_class`] stores them.
    const CLASSES: [RouteClass; 7] = [
        RouteClass::Ingest,
        RouteClass::Lookup,
        RouteClass::Search,
        RouteClass::Stats,
        RouteClass::Stream,
        RouteClass::Mcp,
        RouteClass::Other,
    ];

    /// Records one served request against its class.
    ///
    /// Streams are counted but their duration is discarded, because for a
    /// stream that number is not a latency. A tail lasts as long as a client
    /// chooses to watch and an export as long as its dataset takes to send;
    /// neither says anything about how fast the server is. Recorded, one
    /// dashboard left open overnight would have set the p95 of every latency
    /// panel on the page to eight hours.
    fn observe(&self, class: RouteClass, elapsed_ns: u64, status: u16) {
        let timed = class != RouteClass::Stream;
        if timed {
            self.request_latency.record(elapsed_ns);
        }
        self.requests.increment();
        if let Some(position) = Self::CLASSES.iter().position(|entry| *entry == class) {
            if timed {
                self.by_class[position].record(elapsed_ns);
            } else {
                self.by_class[position].count_only();
            }
        }
        match status {
            200..=299 => self.status_2xx.increment(),
            400..=499 => self.status_4xx.increment(),
            500..=599 => self.status_5xx.increment(),
            _ => {}
        }
    }

    /// Everything the Server screen shows, as JSON.
    ///
    /// The dashboard used to have no way to reach any of this: `/v1/metrics`
    /// speaks Prometheus text, and asking a browser to parse an exposition
    /// format to draw a chart is a parser nobody should have to write.
    fn as_json(&self, engine: &Store) -> Value {
        let engine_metrics = engine.metrics();
        let classes: serde_json::Map<String, Value> = Self::CLASSES
            .iter()
            .zip(self.by_class.iter())
            .map(|(class, latency)| {
                (
                    class.label().to_owned(),
                    json!({
                        "count": latency.count(),
                        "mean_ns": latency.mean_ns(),
                        "max_ns": latency.max_ns(),
                        "p50_ns": latency.percentile_ns(50.0),
                        "p95_ns": latency.percentile_ns(95.0),
                        "p99_ns": latency.percentile_ns(99.0),
                    }),
                )
            })
            .collect();
        json!({
            "uptime_ns": self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            // What an acknowledged write actually guarantees. Absent from this
            // payload before, so the dashboard's fallback rendered every
            // server as `wal` — including a `buffered` one, which promises the
            // opposite. A screen must not be able to invent this.
            "durability": engine.durability().as_str(),
            // The live tail's residency, and both of its bounds, so which one
            // is binding is visible rather than inferred. The ring is the only
            // structure here that holds whole spans indefinitely, so "is the
            // tail why this process is large" has to be answerable.
            "tail_ring": match engine.tail_usage() {
                Some((spans, bytes, max_spans, max_bytes)) => json!({
                    "spans": spans,
                    "bytes": bytes,
                    "max_spans": max_spans,
                    "max_bytes": max_bytes,
                }),
                None => Value::Null,
            },
            "requests": {
                "total": self.requests.get(),
                "rejected": self.rejected.get(),
                "responses_2xx": self.status_2xx.get(),
                "responses_4xx": self.status_4xx.get(),
                "responses_5xx": self.status_5xx.get(),
                "mean_ns": self.request_latency.mean_ns(),
                "max_ns": self.request_latency.max_ns(),
                "p50_ns": self.request_latency.percentile_ns(50.0),
                "p95_ns": self.request_latency.percentile_ns(95.0),
                "p99_ns": self.request_latency.percentile_ns(99.0),
            },
            "by_class": classes,
            "connections": {
                "accepted": self.connections_accepted.get(),
                "refused": self.connections_refused.get(),
                "live": self.connections_live.load(Ordering::Relaxed),
            },
            "decode": {
                "spans": self.decoded_spans.get(),
                "mean_ns": self.decode.mean_ns(),
                "p95_ns": self.decode.percentile_ns(95.0),
            },
            "ingest": {
                "spans_admitted": engine_metrics.spans_admitted.get(),
                "batches_admitted": engine_metrics.batches_admitted.get(),
                "wal_commits": engine_metrics.wal_commits.get(),
                "wal_fsync_p95_ns": engine_metrics.wal_fsync.percentile_ns(95.0),
                "segment_seal_p95_ns": engine_metrics.segment_seal.percentile_ns(95.0),
            },
            "pruning": {
                "segments_examined": engine_metrics.segments_examined.get(),
                "segments_pruned_by_time": engine_metrics.segments_pruned_by_time.get(),
            },
            // Percentiles are bucket upper bounds; state the bound rather than
            // letting a reader assume the figures are exact.
            "percentile_error_bound": 0.0625,
        })
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("traza-server: {error}");
        std::process::exit(1);
    }
}

const USAGE: &str = "Usage: traza-server --data-dir DIR --port PORT [--host ADDR] \
[--profile throughput|balanced|latency (default balanced; sets flush-spans and wal-commit-window; \
NEVER changes durability)] [--ttl-seconds N] [--flush-spans N] \
[--flush-wal-bytes N (seal when the write-ahead log reaches N bytes; 0 disables; \
default 64MiB)] \
[--max-buffer-age-seconds N (seal when the oldest buffered span reaches N seconds; \
0 disables; default 300)] \
[--no-shadow-seal (do not answer observed segment-key shadowing with a \
corrective deduplicating merge)] \
[--max-connections N (default 1024)] [--payload-threshold-bytes N (0 disables)] \
[--durability buffered|wal|flushed (default wal)] \
[--wal-commit-window-us N (delay each fsync so more acks share it; 0 = off)] \
[--compaction-fanout N (0 disables; default 4)] [--compaction-max-segment-bytes N] \
[--no-content-index (content search still works, by scanning)] \
[--tail-ring-spans N (live-tail replay depth; default 8192)] \
[--tail-ring-bytes N (live-tail memory ceiling, whichever bound binds first; \
default 32MiB)] \
[--ui-dir DIR (built dashboard; default: TRAZA_UI_DIR, beside the binary, then ./ui/dist)] \
[--mcp (serve the Model Context Protocol endpoint at /v1/mcp; off by default)] \
[--mcp-annotations (additionally let MCP callers with an rw token record annotations)] \
[--mcp-max-result-bytes N (default 32768)] [--mcp-max-payload-bytes N (default 262144)] \
[--mcp-allowed-origin ORIGIN (repeatable; browser origins allowed to drive /v1/mcp \
besides loopback)] \
[--allow-unauthenticated-non-loopback] [--version] \
[--restore BACKUP_DIR (install a backup into --data-dir, then serve it)]\n\
       traza-server mcp --url URL [--token TOKEN] \
(stdio bridge: speaks MCP on stdin/stdout and forwards to a running server)\n\
       traza-server verify --erasure ID|latest [--data-dir DIR] [--json] \
(offline erasure receipt: re-checks every domain and prints the result of each; \
exits 2 when the erasure does not verify)";

/// Everything the command line decides, with the profile already resolved
/// against the explicit flags.
struct Options {
    data_dir: PathBuf,
    host: String,
    port: u16,
    max_connections: usize,
    allow_unauthenticated_non_loopback: bool,
    ui_dir: Option<PathBuf>,
    /// A backup directory to install before serving; see `--restore`.
    restore_from: Option<PathBuf>,
    profile: Profile,
    compaction_enabled: bool,
    mcp: McpOptions,
    config: Config,
}

/// What the MCP endpoint is allowed to do, resolved from the command line.
///
/// `enabled` defaults to off. A read endpoint that is on by default is a
/// decision to expose every stored prompt to whatever holds the token, and
/// that should be something an operator turned on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct McpOptions {
    /// Off unless `--mcp` was passed.
    enabled: bool,
    annotations: bool,
    limits: traza::mcp::Limits,
    /// Browser origins permitted to drive the endpoint, beyond loopback.
    ///
    /// Operator-supplied, and that is the whole point: the origin cannot be
    /// validated against anything else the request carries, because a request
    /// driven by DNS rebinding supplies all of them.
    allowed_origins: Vec<String>,
}

/// Parses the command line. `Ok(None)` means `--help` was handled and there is
/// nothing to run.
///
/// The profile-owned knobs are collected as `Option`s during the scan and
/// resolved once, afterwards, against the profile. That is what makes
/// precedence independent of argument order: an explicit `--flush-spans` is
/// still `Some` whether it came before or after `--profile`, so it still wins.
/// Resolving as each argument is seen would make `--profile` clobber a flag
/// that preceded it.
fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut data_dir = PathBuf::from("./data");
    let mut host = String::from("127.0.0.1");
    let mut port = 8080_u16;
    let mut ttl_seconds = None;
    let mut payload_threshold_bytes = 256 * 1024_usize;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut allow_unauthenticated_non_loopback = false;
    let mut ui_dir: Option<PathBuf> = None;
    let mut restore_from: Option<PathBuf> = None;
    let mut durability = Durability::default();
    let mut compaction = CompactionConfig::default();
    let mut compaction_enabled = true;
    let mut content_index = true;
    let mut flush_wal_bytes = traza::DEFAULT_FLUSH_WAL_BYTES;
    let mut max_buffer_age_seconds = traza::DEFAULT_MAX_BUFFER_AGE.as_secs();
    let mut shadow_seal = true;
    let mut profile = Profile::default();
    let mut mcp = McpOptions::default();
    // Profile-owned: `None` means "not given on the command line", which is
    // exactly the question the resolve below asks.
    let mut flush_spans: Option<usize> = None;
    let mut tail_ring_spans = traza::DEFAULT_TAIL_RING_SPANS;
    let mut tail_ring_bytes = traza::DEFAULT_TAIL_RING_BYTES;
    let mut wal_commit_window_us: Option<u64> = None;

    let value = |i: usize, name: &str| -> Result<&String, String> {
        args.get(i)
            .ok_or_else(|| format!("{name} requires a value"))
    };
    let number = |i: usize, name: &str| -> Result<u64, String> {
        value(i, name)?
            .parse::<u64>()
            .map_err(|_| format!("{name} must be a non-negative number"))
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(value(i, "--data-dir")?);
            }
            "--host" => {
                i += 1;
                host = value(i, "--host")?.clone();
            }
            "--port" => {
                i += 1;
                port = value(i, "--port")?
                    .parse()
                    .map_err(|_| "--port must be a port number".to_owned())?;
            }
            "--ttl-seconds" => {
                i += 1;
                ttl_seconds = Some(number(i, "--ttl-seconds")?);
            }
            "--max-buffer-age-seconds" => {
                i += 1;
                max_buffer_age_seconds = number(i, "--max-buffer-age-seconds")?;
            }
            "--no-shadow-seal" => {
                shadow_seal = false;
            }
            "--payload-threshold-bytes" => {
                i += 1;
                payload_threshold_bytes = number(i, "--payload-threshold-bytes")? as usize;
            }
            "--flush-spans" => {
                i += 1;
                flush_spans = Some((number(i, "--flush-spans")? as usize).max(1));
            }
            "--flush-wal-bytes" => {
                i += 1;
                flush_wal_bytes = number(i, "--flush-wal-bytes")?;
            }
            "--tail-ring-spans" => {
                i += 1;
                tail_ring_spans = (number(i, "--tail-ring-spans")? as usize).max(1);
            }
            "--tail-ring-bytes" => {
                i += 1;
                tail_ring_bytes = (number(i, "--tail-ring-bytes")? as usize).max(1);
            }
            "--max-connections" => {
                i += 1;
                max_connections = (number(i, "--max-connections")? as usize).max(1);
            }
            "--compaction-fanout" => {
                i += 1;
                let fanout = number(i, "--compaction-fanout")? as usize;
                // 0 or 1 cannot merge anything; treat as "off" rather than
                // silently looping.
                compaction_enabled = fanout >= 2;
                compaction.fanout = fanout.max(2);
            }
            "--compaction-max-segment-bytes" => {
                i += 1;
                compaction.max_segment_bytes = number(i, "--compaction-max-segment-bytes")?;
            }
            "--wal-commit-window-us" => {
                i += 1;
                wal_commit_window_us = Some(number(i, "--wal-commit-window-us")?);
            }
            // Content search still works without it — segments are scanned
            // rather than skipped — so this trades query latency for seal CPU
            // and about 1-2% of segment size.
            "--no-content-index" => content_index = false,
            "--durability" => {
                i += 1;
                let name = value(i, "--durability")?;
                durability = Durability::parse(name)
                    .ok_or_else(|| "--durability must be buffered|wal|flushed".to_owned())?;
            }
            "--profile" => {
                i += 1;
                let name = value(i, "--profile")?;
                profile = Profile::parse(name)
                    .ok_or_else(|| "--profile must be throughput|balanced|latency".to_owned())?;
            }
            "--ui-dir" => {
                i += 1;
                ui_dir = Some(PathBuf::from(value(i, "--ui-dir")?));
            }
            "--restore" => {
                i += 1;
                restore_from = Some(PathBuf::from(value(i, "--restore")?));
            }
            "--mcp" => {
                mcp.enabled = true;
            }
            // Implies --mcp: asking for the write tool and getting no endpoint
            // at all is a silent no-op, and the alternative reading — "enable
            // annotations on the endpoint I did not enable" — is not one
            // anybody means.
            "--mcp-annotations" => {
                mcp.enabled = true;
                mcp.annotations = true;
            }
            "--mcp-max-result-bytes" => {
                i += 1;
                mcp.limits.max_result_bytes = number(i, "--mcp-max-result-bytes")? as usize;
            }
            "--mcp-max-payload-bytes" => {
                i += 1;
                mcp.limits.max_payload_bytes = number(i, "--mcp-max-payload-bytes")? as usize;
            }
            "--mcp-allowed-origin" => {
                i += 1;
                mcp.enabled = true;
                let origin = value(i, "--mcp-allowed-origin")?
                    .trim()
                    .to_ascii_lowercase();
                if !origin.contains("://") {
                    return Err(format!(
                        "--mcp-allowed-origin must be a whole origin including the scheme, \
                         like https://traza.example.com (got {origin})"
                    ));
                }
                mcp.allowed_origins
                    .push(origin.trim_end_matches('/').to_owned());
            }
            "--allow-unauthenticated-non-loopback" => {
                allow_unauthenticated_non_loopback = true;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("traza-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    // A ceiling too small to hold a schema-conforming result is refused here
    // rather than producing, at every request, an answer a validating client
    // rejects. Same stance as an invalid TRAZA_TOKENS: fail where the operator
    // can see it.
    if mcp.enabled && mcp.limits.max_result_bytes < traza::mcp::MIN_RESULT_BYTES {
        return Err(format!(
            "--mcp-max-result-bytes {} is below the {} needed for a result that both fits the \
             ceiling and conforms to the output schema its tool advertises",
            mcp.limits.max_result_bytes,
            traza::mcp::MIN_RESULT_BYTES,
        ));
    }

    Ok(Some(Options {
        data_dir,
        host,
        port,
        max_connections,
        allow_unauthenticated_non_loopback,
        ui_dir,
        restore_from,
        profile,
        compaction_enabled,
        mcp,
        config: Config {
            // Explicit flag beats profile beats built-in default, in that
            // order and regardless of where each appeared.
            flush_spans: flush_spans.unwrap_or_else(|| profile.flush_spans()),
            // 0 removes the byte bound; the record bounds still apply.
            flush_wal_bytes: (flush_wal_bytes > 0).then_some(flush_wal_bytes),
            wal_commit_window: match wal_commit_window_us {
                // An explicit 0 is a real answer ("no window"), not an absent
                // one, so it overrides a profile that wanted one.
                Some(micros) => (micros > 0).then(|| Duration::from_micros(micros)),
                None => profile.wal_commit_window(),
            },
            ttl_seconds,
            // 0 removes the age bound; the buffer is then bounded by volume
            // alone, which a trickle workload never reaches.
            max_buffer_age: (max_buffer_age_seconds > 0)
                .then(|| Duration::from_secs(max_buffer_age_seconds)),
            shadow_seal,
            // 0 disables offloading.
            payload_threshold: (payload_threshold_bytes > 0).then_some(payload_threshold_bytes),
            // Never profile-derived: what an acknowledgement guarantees is a
            // contract with clients, not a performance setting.
            durability,
            content_index,
            compaction: compaction_enabled.then_some(compaction),
            tail_ring_spans,
            tail_ring_bytes,
        },
    }))
}

/// `traza-server verify --erasure <id|latest>`: the offline erasure receipt.
///
/// Opens the store the way serving would — the standard recovery: log
/// replayed, torn tails healed, pending erasures masked — then re-checks
/// every domain the subject's bytes could inhabit and prints the result of
/// each. It never runs the purge itself: a pending erasure verifies as
/// incomplete, which is the truthful answer until a serving process settles
/// it. Exits 0 when the receipt says erased, 2 when it does not.
///
/// Refuses a directory a live server owns; that server's
/// `GET /v1/erasures/<id>/verify` is the same receipt without the contention.
fn run_verify(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("./data");
    let mut erasure: Option<String> = None;
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir =
                    PathBuf::from(args.get(i).ok_or("--data-dir requires a value")?.as_str());
            }
            "--erasure" => {
                i += 1;
                erasure = Some(
                    args.get(i)
                        .ok_or("--erasure requires an id or 'latest'")?
                        .clone(),
                );
            }
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }
    let Some(erasure) = erasure else {
        return Err("verify requires --erasure <id|latest>".into());
    };

    let store = match Store::open(&data_dir, Config::default()) {
        Ok(store) => store,
        Err(traza::Error::AlreadyOpen) => {
            return Err(format!(
                "{} is owned by a live traza-server; ask that server instead: \
                 GET /v1/erasures/<id>/verify returns the same receipt",
                data_dir.display()
            )
            .into())
        }
        Err(error) => return Err(error.into()),
    };

    let id = match erasure.as_str() {
        "latest" => store
            .erasures()?
            .last()
            .map(|status| status.erase.id)
            .ok_or("the tombstone log records no erasures")?,
        text => text
            .parse::<u64>()
            .map_err(|_| format!("--erasure takes a decimal id or 'latest' (got {text:?})"))?,
    };
    let receipt = store.verify_erasure(id)?;
    match as_json {
        true => println!(
            "{}",
            serde_json::to_string_pretty(&receipt).unwrap_or_else(|_| "{}".to_owned())
        ),
        false => print!("{}", receipt.render_text()),
    }
    if receipt.result != "erased" {
        std::process::exit(2);
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `traza-server mcp --url ...` is the stdio bridge, not a server. It opens
    // no data directory and holds no state — the running server it forwards to
    // is the only thing that does.
    if args.first().map(String::as_str) == Some("mcp") {
        return run_mcp_bridge(&args[1..]);
    }
    // `traza-server verify --erasure ...` produces the erasure receipt
    // offline, against a data directory no live server owns.
    if args.first().map(String::as_str) == Some("verify") {
        return run_verify(&args[1..]);
    }
    let Some(options) = parse_args(&args)? else {
        return Ok(());
    };
    let Options {
        data_dir,
        host,
        port,
        max_connections,
        allow_unauthenticated_non_loopback,
        ui_dir,
        restore_from,
        profile,
        compaction_enabled,
        mcp,
        config,
    } = options;
    let durability = config.durability;
    let ttl_seconds = config.ttl_seconds;

    // Bearer auth from TRAZA_TOKENS; unset is allowed on loopback by default.
    // A set-but-invalid value refuses startup — running open when the operator
    // tried to configure auth would be the worst failure mode.
    let auth_config = traza::auth::AuthConfig::from_env()
        .map_err(|error| format!("auth configuration: {error}"))?;
    if auth_config.is_none() && !is_loopback_bind(&host) && !allow_unauthenticated_non_loopback {
        return Err(format!(
            "refusing unauthenticated non-loopback bind {host}; configure TRAZA_TOKENS or pass --allow-unauthenticated-non-loopback explicitly"
        )
        .into());
    }
    let auth = Arc::new(auth_config);
    let metrics = Arc::new(ServerMetrics::default());
    // In-flight 503 refusals, bounded so a connection flood cannot spawn
    // without limit.
    let refusals = Arc::new(AtomicUsize::new(0));
    // Restore installs a backup before the store opens, because it replaces
    // the working set wholesale — the backup is verified first, and the swap
    // commits at one `CURRENT` rename, so a failed restore leaves what was
    // there rather than a blend.
    let engine = Arc::new(match &restore_from {
        Some(backup) => {
            eprintln!(
                "traza-server: restoring {} into {}",
                backup.display(),
                data_dir.display()
            );
            let store = Store::restore(&data_dir, backup, config.clone())?;
            eprintln!(
                "traza-server: restored generation {}",
                store.live_generation()
            );
            store
        }
        None => Store::open(&data_dir, config.clone())?,
    });

    // TTL enforcement, segment compaction, and the buffer's non-volume bounds
    // live in the engine; the server only schedules them. Compaction ticks far
    // more often than TTL: it is what keeps filtered-search latency flat as
    // segments accumulate, and it is a no-op when no run qualifies. Buffer
    // maintenance shares the fast tick because a seal it declines to schedule
    // costs a comparison, and the age bound's whole promise is that a store
    // that went quiet still seals within it.
    // The tick always runs now: besides the conditional duties above it is
    // the erasure-resume loop, and a pending erasure — one a crash
    // interrupted — must settle even on a store with TTL, compaction and the
    // buffer bounds all switched off.
    {
        let maintainer = Arc::clone(&engine);
        let ttl_enabled = ttl_seconds.is_some();
        let _ = compaction_enabled;
        thread::Builder::new()
            .name("traza-maintenance".into())
            .spawn(move || {
                let mut ticks = 0u64;
                loop {
                    thread::sleep(std::time::Duration::from_secs(5));
                    ticks += 1;
                    if let Err(error) = maintainer.maintain_buffer() {
                        eprintln!("buffer maintenance failed: {error}");
                    }
                    // An erasure a crash interrupted is pending in the
                    // tombstone log: its subject is already masked, and this
                    // finishes the purge and settles it. A comparison and an
                    // empty list on every tick that has nothing to do.
                    match maintainer.resume_erasures() {
                        Ok(0) => {}
                        Ok(resumed) => eprintln!(
                            "traza-server: settled {resumed} erasure(s) interrupted by a restart"
                        ),
                        Err(error) => eprintln!("erasure resume failed: {error}"),
                    }
                    if let Err(error) = maintainer.compact_segments() {
                        eprintln!("segment compaction failed: {error}");
                    }
                    // TTL keeps its documented one-minute cadence.
                    if ttl_enabled && ticks % 12 == 0 {
                        if let Err(error) = maintainer.compact_expired() {
                            eprintln!("expiry compaction failed: {error}");
                        }
                    }
                    // A checkpoint every five minutes. Nothing depends on the
                    // cadence for correctness — recovery excludes folded
                    // frames by stamp whether or not one has run recently —
                    // but each one moves `folded_through` forward, which is
                    // what bounds replay on the next restart, and it keeps a
                    // long-lived store's live generation describing something
                    // close to what is actually on disk. Cheap by
                    // construction: immutable segments carry their digests
                    // over from the previous manifest, so only what was
                    // written since is hashed.
                    if ticks % 60 == 0 {
                        if let Err(error) = maintainer.checkpoint() {
                            eprintln!("checkpoint failed: {error}");
                        }
                    }
                }
            })?;
    }

    // --port 0 binds an ephemeral port; the actual port is announced on
    // stderr so process-level tests can discover it.
    let listener = TcpListener::bind((host.as_str(), port))?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("traza-server listening on {host}:{actual_port}");
    // The acknowledgement contract is announced, not implied.
    match durability {
        Durability::Buffered => eprintln!(
            "traza-server: durability=buffered — acknowledged writes are IN MEMORY ONLY and \
             a crash loses anything not yet flushed. Use --durability wal in production."
        ),
        Durability::Wal => eprintln!(
            "traza-server: durability=wal — acknowledged writes are fsynced to the \
             write-ahead log and recovered on restart"
        ),
        Durability::Flushed => eprintln!(
            "traza-server: durability=flushed — acknowledged writes are sealed into a segment"
        ),
    }
    // The resolved values, not the profile name: a profile whose knobs were
    // partly overridden by explicit flags is not the profile, and an operator
    // reading the log should see what is actually in force.
    eprintln!(
        "traza-server: profile={} — flush-spans={}, wal-commit-window={}",
        profile.as_str(),
        config.flush_spans,
        match config.wal_commit_window {
            Some(window) => format!("{}us", window.as_micros()),
            None => "off".to_owned(),
        }
    );

    // What the agent-facing surface will actually do, announced rather than
    // left to be discovered by a client that gets a 404.
    if mcp.enabled {
        eprintln!(
            "traza-server: MCP endpoint on {} — read tools{}; results capped at {} bytes",
            traza::mcp::ENDPOINT,
            if mcp.annotations {
                " plus record_annotation for rw tokens"
            } else {
                " only (no --mcp-annotations)"
            },
            mcp.limits.max_result_bytes,
        );
    }

    // The dashboard is served from disk (ui/ `npm run build` output), never
    // compiled in. A missing build is not fatal: the API runs, and the UI
    // routes explain how to produce it.
    let mcp = Arc::new(mcp);
    let ui = Arc::new(match ui_dir {
        Some(explicit) => traza::ui::UiRoot::new(explicit),
        None => traza::ui::UiRoot::discover(),
    });
    if ui.is_available() {
        eprintln!(
            "traza-server serving dashboard from {}",
            ui.directory().display()
        );
    } else {
        // Name every path tried: "no dashboard at ./ui/dist" tells an operator
        // running an installed binary from some other directory nothing at all.
        let searched = ui.searched();
        if searched.is_empty() {
            eprintln!("traza-server: no dashboard at {}", ui.directory().display());
        } else {
            eprintln!("traza-server: no dashboard found; looked in:");
            for path in searched {
                eprintln!("  {}", path.display());
            }
        }
        eprintln!(
            "traza-server: the API is unaffected. Build it with `cd ui && npm ci && npm run build`, \
             or point --ui-dir (or TRAZA_UI_DIR) at a built copy."
        );
    }

    // One thread per connection, hard-bounded.
    //
    // This replaced a fixed worker pool fed by a queue. With keep-alive a
    // connection occupies its handler until the client closes it, so a pool of
    // N threads serves at most N clients and the rest sit in the queue getting
    // nothing — indistinguishable from a hang. Bounding CONNECTIONS instead
    // makes the limit explicit and lets the server say so (503) rather than
    // stall. Threads are cheap here because a keep-alive connection amortizes
    // its spawn over every request it carries.
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("accept failed: {error}");
                continue;
            }
        };
        let live = metrics.connections_live.load(Ordering::Relaxed);
        if live >= max_connections {
            metrics.connections_refused.increment();
            // Refuse loudly. Accepting and queueing would hide the overload as
            // latency, which is what makes an overloaded server undiagnosable.
            //
            // The refusal runs on its own thread because delivering it takes
            // time: closing a socket while the client's request bytes sit
            // unread in the receive queue makes the kernel send RST, which
            // discards the 503 the client was supposed to read. So the body
            // has to be written, the write side shut down, and the inbound
            // bytes drained — none of which may block the accept loop.
            // Refusal threads are themselves bounded; past that, dropping the
            // connection outright is the correct degradation under a flood.
            if refusals.load(Ordering::Relaxed) < MAX_REFUSAL_THREADS {
                refusals.fetch_add(1, Ordering::Relaxed);
                let counter = Arc::clone(&refusals);
                let spawned = thread::Builder::new()
                    .name("traza-refuse".into())
                    .spawn(move || {
                        refuse_connection(stream);
                        counter.fetch_sub(1, Ordering::Relaxed);
                    });
                if spawned.is_err() {
                    refusals.fetch_sub(1, Ordering::Relaxed);
                }
            }
            continue;
        }
        metrics.connections_live.fetch_add(1, Ordering::Relaxed);
        metrics.connections_accepted.increment();
        let connection_engine = Arc::clone(&engine);
        let connection_auth = Arc::clone(&auth);
        let connection_ui = Arc::clone(&ui);
        let connection_metrics = Arc::clone(&metrics);
        let connection_mcp = Arc::clone(&mcp);
        let spawned = thread::Builder::new()
            .name("traza-http".into())
            .spawn(move || {
                let _ = handle_connection(
                    stream,
                    &connection_engine,
                    &connection_auth,
                    &connection_ui,
                    &connection_metrics,
                    &connection_mcp,
                );
                connection_metrics
                    .connections_live
                    .fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            // Out of threads: undo the reservation so the count stays honest.
            metrics.connections_live.fetch_sub(1, Ordering::Relaxed);
            metrics.connections_refused.increment();
            eprintln!("failed to spawn a connection handler");
        }
    }
    Ok(())
}

/// The stdio bridge: MCP on stdin/stdout, forwarded to a running server.
///
/// Many MCP clients still launch their servers as a subprocess and speak over
/// pipes. This is that subprocess, and it does exactly one thing — translate
/// framing. No caching, no tool logic, no state of its own. If it ever needs a
/// second responsibility, that is evidence the split is wrong.
///
/// Two rules from the transport are load-bearing here: **stdout carries
/// nothing but MCP messages** (every diagnostic goes to stderr, or the client's
/// parser breaks), and a message the server answers with `202` produces no
/// output at all.
fn run_mcp_bridge(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    const BRIDGE_USAGE: &str =
        "Usage: traza-server mcp --url http://HOST:PORT[/v1/mcp] [--token TOKEN]";
    let mut url = None;
    let mut token = std::env::var("TRAZA_TOKEN").ok().filter(|t| !t.is_empty());
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--url" => {
                index += 1;
                url = Some(args.get(index).ok_or("--url requires a value")?.clone());
            }
            "--token" => {
                index += 1;
                token = Some(args.get(index).ok_or("--token requires a value")?.clone());
            }
            "--help" | "-h" => {
                println!("{BRIDGE_USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}\n{BRIDGE_USAGE}").into()),
        }
        index += 1;
    }
    let url = url.ok_or(BRIDGE_USAGE)?;
    let (host, port, path) = split_url(&url)?;
    let path = if path == "/" {
        traza::mcp::ENDPOINT.to_owned()
    } else {
        path
    };
    eprintln!("traza-server mcp: bridging stdio to http://{host}:{port}{path}");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            // The client closed our input: the documented way a stdio session
            // ends.
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match forward(&host, port, &path, token.as_deref(), trimmed.as_bytes()) {
            Ok(Some(response)) => {
                stdout.write_all(response.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            // 202: accepted, nothing to say.
            Ok(None) => {}
            Err(error) => {
                eprintln!("traza-server mcp: {error}");
                // A transport failure still owes the client a reply, or it
                // waits for one that will never come. Only a request has an
                // id to answer; a notification that failed is only a log line.
                if let Some(id) = serde_json::from_str::<Value>(trimmed)
                    .ok()
                    .and_then(|message| message.get("id").cloned())
                    .filter(|id| !id.is_null())
                {
                    let body = traza::mcp::error_response(
                        id,
                        -32603,
                        &format!("traza bridge could not reach the server: {error}"),
                    );
                    stdout.write_all(body.to_string().as_bytes())?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
            }
        }
    }
}

/// One request/response exchange with the server. `Ok(None)` is a `202`.
fn forward(
    host: &str,
    port: u16,
    path: &str,
    token: Option<&str>,
    body: &[u8],
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let authorization = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {}\r\n\
         {authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        traza::mcp::PROTOCOL_VERSION,
        body.len(),
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = find_bytes(&response, b"\r\n\r\n").ok_or("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or("malformed HTTP status line")?;
    let payload = String::from_utf8_lossy(&response[split + 4..])
        .trim()
        .to_owned();
    match status {
        202 => Ok(None),
        200 => Ok(Some(payload)),
        // Everything else is the server refusing: surface its own words rather
        // than inventing a reason.
        other => Err(format!("server returned {other}: {payload}").into()),
    }
}

/// Splits `scheme://host:port/path` into its parts, defaulting the port by
/// scheme and the path to `/`.
fn split_url(url: &str) -> Result<(String, u16, String), Box<dyn std::error::Error>> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    if scheme == "https" {
        return Err(
            "the bridge speaks plain HTTP; put TLS termination in front of it, and \
                    point --url at the plaintext side"
                .into(),
        );
    }
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, String::new()), |(a, p)| (a, format!("/{p}")));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse::<u16>()?),
        None => (authority.to_owned(), 80),
    };
    if host.is_empty() {
        return Err("--url needs a host".into());
    }
    Ok((
        host,
        port,
        if path.is_empty() {
            "/".to_owned()
        } else {
            path
        },
    ))
}

fn is_loopback_bind(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Serves requests on one connection until the client or the server ends it.
///
/// Each iteration decides, before it answers, whether the connection can
/// survive the response — the `Connection:` header the client sees and what
/// this loop actually does must agree, or the client's framing breaks.
fn handle_connection(
    stream: TcpStream,
    engine: &Store,
    auth: &Option<traza::auth::AuthConfig>,
    ui: &traza::ui::UiRoot,
    metrics: &ServerMetrics,
    mcp: &McpOptions,
) -> io::Result<()> {
    // A silent or dribbling peer must not park this thread forever.
    let timeout = socket_timeout();
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // Ingest is many small writes; without this each response pays a round
    // trip for the client's ACK before the next one goes out.
    let _ = stream.set_nodelay(true);
    let mut connection = Connection::new(stream);

    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        let head = match connection.read_head() {
            Ok(Some(head)) => head,
            // A clean close at a request boundary is how keep-alive ends.
            Ok(None) => return Ok(()),
            Err(error) => {
                // The head did not parse, so where the body ends is unknown
                // and nothing further on this socket can be trusted.
                let mut responder = connection.responder(false);
                return responder.json(400, json!({"error": error.to_string()}));
            }
        };
        let started = std::time::Instant::now();
        // A request answered WITHOUT reading its body leaves those bytes in
        // the socket, where they would be read as the next request. Any such
        // path must close.
        let body_is_unread = head.content_length > 0;
        let keep_alive = head.wants_keep_alive();

        // The dashboard SHELL is served before the auth gate: the page must
        // load in a browser without credentials, while every /v1 call it makes
        // below stays gated (the page attaches the bearer token itself).
        // Static assets carry no stored data, so this leaks nothing.
        if head.method == "GET" {
            let path = percent_decode(
                head.target
                    .split_once('?')
                    .map_or(head.target.as_str(), |(path, _)| path),
            );
            if let Some(file) = ui.resolve(&path) {
                let mut responder = connection.responder(keep_alive && !body_is_unread);
                responder.file(&file)?;
                if responder.keep_alive {
                    continue;
                }
                return Ok(());
            }
            if matches!(path.as_str(), "/" | "/dashboard" | "/dashboard/") {
                let mut responder = connection.responder(keep_alive && !body_is_unread);
                responder.json(
                    404,
                    json!({
                        "error": "no dashboard build found",
                        "next": format!(
                            "build it with: cd ui && npm ci && npm run build (serving {})",
                            ui.directory().display()
                        ),
                    }),
                )?;
                if responder.keep_alive {
                    continue;
                }
                return Ok(());
            }
        }
        // Auth verdicts need only the head: rejecting BEFORE the body read
        // means an unauthenticated client cannot make this server buffer
        // 64 MiB. The connection then closes, precisely because that body was
        // never read.
        //
        // The MCP endpoint authenticates the same way and authorizes
        // differently: it carries reads and writes alike over one POST, so the
        // method rule would either refuse every `ro` token or grant every
        // caller the write scope. Its token's scope is resolved here and
        // enforced per tool.
        let is_mcp = head
            .target
            .split_once('?')
            .map_or(head.target.as_str(), |(path, _)| path)
            == traza::mcp::ENDPOINT;
        let mut access = traza::mcp::Access::ReadWrite;
        if let Some(config) = auth {
            let verdict = if is_mcp {
                config
                    .scope_for(head.authorization.as_deref())
                    .map(|scope| match scope {
                        traza::auth::Scope::ReadOnly => traza::mcp::Access::Read,
                        traza::auth::Scope::ReadWrite => traza::mcp::Access::ReadWrite,
                    })
            } else {
                config
                    .authorize(head.authorization.as_deref(), &head.method)
                    .map(|()| traza::mcp::Access::ReadWrite)
            };
            match verdict {
                Ok(granted) => access = granted,
                Err(failure) => {
                    metrics.rejected.increment();
                    let challenge = failure
                        .www_authenticate()
                        .map(|value| format!("WWW-Authenticate: {value}\r\n"))
                        .unwrap_or_default();
                    let body = failure.body();
                    let reason = if failure.status() == 401 {
                        "Unauthorized"
                    } else {
                        "Forbidden"
                    };
                    let mut responder = connection.responder(false);
                    let head = format!(
                        "HTTP/1.1 {} {reason}\r\n{challenge}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        failure.status(),
                        body.len(),
                    );
                    return responder.raw(&head, body.as_bytes());
                }
            }
        }
        let body = match connection.read_body(&head) {
            Ok(body) => body,
            Err(error) => {
                let mut responder = connection.responder(false);
                return responder.json(400, json!({"error": error.to_string()}));
            }
        };
        let request = Request {
            method: head.method,
            target: head.target,
            content_type: head.content_type,
            origin: head.origin,
            mcp_protocol_version: head.mcp_protocol_version,
            body,
        };
        let mut responder = connection.responder(keep_alive);
        let class = RouteClass::of(&request.method, {
            let target = &request.target;
            target
                .split_once('?')
                .map_or(target.as_str(), |(path, _)| path)
        });
        let result = serve_request(&mut responder, request, engine, metrics, mcp, access);
        let persist = responder.keep_alive;
        let status = responder.status;
        // Observed BEFORE the error is propagated, because a write failure is
        // still a request the server served. It also used to make streams
        // uncountable: a tail ends when its client disconnects, so the write
        // that discovers the disconnect always errors, and every tail ever
        // served left the counter at zero.
        metrics.observe(
            class,
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            status.unwrap_or(200),
        );
        result?;
        if !persist {
            return Ok(());
        }
    }
    Ok(())
}

/// Decodes an ingest body straight into spans.
///
/// Deliberately NOT via `serde_json::Value`: parsing to a DOM, cloning the
/// array out of it, and then re-walking that DOM per span made three passes
/// and three sets of allocations out of what serde can do in one. On the
/// ingest hot path that was the single largest server-side cost.
///
/// The shape is either a bare array or `{"spans": [...]}`, distinguished by
/// the first non-whitespace byte rather than by trying one and falling back —
/// a failed attempt would have to re-parse from the start.
fn decode_spans(body: &[u8]) -> Result<Vec<Span>, String> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        spans: Vec<Span>,
    }
    match body.iter().find(|byte| !byte.is_ascii_whitespace()) {
        Some(b'[') => serde_json::from_slice::<Vec<Span>>(body).map_err(|error| error.to_string()),
        Some(b'{') => serde_json::from_slice::<Envelope>(body)
            .map(|envelope| envelope.spans)
            .map_err(|error| error.to_string()),
        _ => Err("body must be an array or {spans: [...]}".to_owned()),
    }
}

fn serve_request(
    responder: &mut Responder<'_>,
    request: Request,
    engine: &Store,
    metrics: &ServerMetrics,
    mcp: &McpOptions,
    access: traza::mcp::Access,
) -> io::Result<()> {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    if path == traza::mcp::ENDPOINT {
        return serve_mcp(responder, &request, engine, mcp, access);
    }
    match (request.method.as_str(), path) {
        ("POST", "/v1/spans") => {
            let spans = match metrics.decode.time(|| decode_spans(&request.body)) {
                Ok(spans) => spans,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            metrics.decoded_spans.add(spans.len() as u64);
            // Both halves of the (trace_id, span_id) primary key must be
            // non-empty: an empty span_id would make every such span in a
            // trace one colliding key, silently upserted over each other while
            // the response counts them all as accepted.
            for (index, span) in spans.iter().enumerate() {
                if span.trace_id.is_empty() {
                    return responder.json(
                        400,
                        json!({"error": format!("span {index}: trace_id is empty")}),
                    );
                }
                if span.span_id.is_empty() {
                    return responder.json(
                        400,
                        json!({"error": format!("span {index}: span_id is empty")}),
                    );
                }
            }
            let accepted = spans.len();
            match engine.ingest_batch(spans) {
                Ok(()) => responder.json(
                    200,
                    // The client should never have to guess what a 200 promises.
                    json!({"accepted": accepted, "durability": engine.durability().as_str()}),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/traces") => {
            // OTLP/HTTP: an ExportTraceServiceRequest, binary protobuf or
            // JSON by Content-Type. Each encoding decodes straight to spans;
            // the mapping rules they must agree on are shared rather than
            // duplicated (docs: README, src/otlp.rs).
            let is_protobuf = request.content_type.starts_with("application/x-protobuf");
            // Timed as one stage: this covers the wire decode AND the
            // OTLP-to-Span mapping, which is the whole cost of accepting a
            // batch on this route.
            let decoded = metrics.decode.time(|| {
                if is_protobuf {
                    traza::otlp_pb::spans_from_protobuf(&request.body)
                        .map_err(|error| error.to_string())
                } else {
                    traza::otlp::spans_from_json(&request.body).map_err(|error| error.to_string())
                }
            });
            let spans = match decoded {
                Ok(spans) => spans,
                Err(error) => {
                    return responder.json(400, json!({"error": error}));
                }
            };
            metrics.decoded_spans.add(spans.len() as u64);
            match engine.ingest_batch(spans) {
                Ok(()) if is_protobuf => {
                    // An empty ExportTraceServiceResponse is zero protobuf
                    // bytes; protobuf clients expect the matching media type.
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: {}\r\n\r\n",
                        responder.connection_header()
                    );
                    responder.raw(&head, b"")
                }
                Ok(()) => responder.json(200, json!({"partialSuccess": {}})),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/flush") => match engine.flush() {
            Ok(()) => responder.json(200, json!({"flushed": true})),
            Err(error) => responder.json(503, json!({"error": error.to_string()})),
        },
        // Publishing a generation is what makes a deletion, a seal and an
        // annotation one durable fact rather than four. Operators need it for
        // backup; the maintenance thread runs it on its own cadence.
        ("POST", "/v1/checkpoint") => match engine.checkpoint() {
            Ok(generation) => responder.json(200, json!({"generation": generation})),
            Err(error) => responder.json(503, json!({"error": error.to_string()})),
        },
        // Backup, with the server still running: pin the live generation as a
        // hard-link farm, verify it, and hand back the path to copy. The copy
        // is the operator's to make — Traza will not write outside its own
        // data directory — and `release` frees the pin when they are done.
        ("GET", "/v1/verify") => {
            let generation = engine.live_generation();
            match engine.verify_generation(generation) {
                Ok(problems) => responder.json(
                    200,
                    json!({
                        "generation": generation,
                        "intact": problems.is_empty(),
                        "problems": problems,
                    }),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("POST", path) if path.starts_with("/v1/backups/") => {
            let rest = &path["/v1/backups/".len()..];
            let (label, release) = match rest.strip_suffix("/release") {
                Some(label) => (label, true),
                None => (rest, false),
            };
            if release {
                return match engine.release_pin(label) {
                    Ok(()) => responder.json(200, json!({"released": true, "backup": label})),
                    Err(error) => responder.json(503, json!({"error": error.to_string()})),
                };
            }
            match engine.pin_generation(label) {
                // Verified before it is reported, because a backup nobody
                // checked is a backup nobody can trust.
                Ok(generation) => match engine.verify_pin(label) {
                    Ok(problems) if problems.is_empty() => responder.json(
                        201,
                        json!({
                            "backup": label,
                            "generation": generation,
                            "path": engine.pin_path(label).display().to_string(),
                            "verified": true,
                        }),
                    ),
                    Ok(problems) => responder.json(
                        500,
                        json!({"error": "the pinned backup does not verify", "problems": problems}),
                    ),
                    Err(error) => responder.json(503, json!({"error": error.to_string()})),
                },
                Err(error) => responder.json(409, json!({"error": error.to_string()})),
            }
        }
        // Targeted deletion, with the receipt to prove it. POST names a
        // subject and blocks until the erasure settles — resolve, tombstone,
        // purge every domain, checkpoint — and answers with the settle
        // summary. GET lists the tombstone log; GET /{id}/verify re-checks
        // every domain and returns the receipt. Erasure rides the ordinary
        // method rule (POST needs the write scope), and there is deliberately
        // NO MCP tool for it: the agent-facing surface stays read-only, so
        // stored adversarial text has no deletion verb to actuate.
        ("POST", "/v1/erasures") => {
            let body: Value = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(error) => return responder.json(400, json!({"error": error.to_string()})),
            };
            let Some(subject) = body.get("subject").cloned() else {
                return responder.json(
                    400,
                    json!({"error": "body must be {\"subject\": {\"kind\": \"trace\"|\"span\"|\"session\"|\"payload\", ...}}"}),
                );
            };
            let subject: traza::erasure::Subject = match serde_json::from_value(subject) {
                Ok(subject) => subject,
                Err(error) => {
                    return responder.json(
                        400,
                        json!({"error": format!("subject does not parse: {error}")}),
                    )
                }
            };
            match engine.erase(subject) {
                Ok(status) => responder.json(
                    200,
                    serde_json::to_value(&status).unwrap_or_else(|_| json!({})),
                ),
                Err(traza::Error::InvalidSpan(reason)) => {
                    responder.json(400, json!({"error": reason}))
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/erasures") => match engine.erasures() {
            Ok(erasures) => responder.json(
                200,
                json!({
                    "erasures": serde_json::to_value(erasures)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))
                }),
            ),
            Err(error) => responder.json(503, json!({"error": error.to_string()})),
        },
        ("GET", path) if path.starts_with("/v1/erasures/") && path.ends_with("/verify") => {
            let id = &path["/v1/erasures/".len()..path.len() - "/verify".len()];
            let Ok(id) = id.parse::<u64>() else {
                return responder.json(400, json!({"error": "erasure ids are decimal integers"}));
            };
            match engine.verify_erasure(id) {
                Ok(receipt) => responder.json(
                    200,
                    serde_json::to_value(&receipt).unwrap_or_else(|_| json!({})),
                ),
                Err(traza::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    responder.json(404, json!({"error": error.to_string()}))
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", path) if path.starts_with("/v1/erasures/") => {
            let id = &path["/v1/erasures/".len()..];
            let Ok(id) = id.parse::<u64>() else {
                return responder.json(400, json!({"error": "erasure ids are decimal integers"}));
            };
            match engine.erasure_status(id) {
                Ok(Some(status)) => responder.json(
                    200,
                    serde_json::to_value(&status).unwrap_or_else(|_| json!({})),
                ),
                Ok(None) => responder.json(
                    404,
                    json!({"error": format!("no erasure {id} is recorded in the tombstone log")}),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/spans") => {
            let (filter, cursor) = match span_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            let limit = filter.limit;
            match engine.query_costed(&filter, cursor.as_ref()) {
                Ok((spans, cost)) => {
                    // A cursor is offered only when the page came back full.
                    // A short page has already reached the end of the match
                    // set, and handing out a cursor there would invite a
                    // round trip that can only return nothing.
                    let next = limit
                        .filter(|limit| spans.len() >= *limit)
                        .and_then(|_| spans.last())
                        .map(|span| traza::SpanCursor::from(span).to_token());
                    responder.json(
                        200,
                        json!({
                            "spans": serde_json::to_value(spans)
                                .unwrap_or_else(|_| Value::Array(Vec::new())),
                            "next_cursor": next,
                            // What the query actually touched. The dashboard
                            // shows this rather than asserting the store is
                            // fast; a time filter that prunes nothing is then
                            // visible instead of merely disappointing.
                            "cost": {
                                "elapsed_ns": cost.elapsed_ns,
                                "segments_examined": cost.segments_examined,
                                "segments_pruned": cost.segments_pruned,
                            },
                        }),
                    )
                }
                // A too-broad query is the caller's to fix; 503 would tell
                // them to retry with backoff, and retrying cannot help.
                Err(error @ traza::Error::QueryTooBroad(_)) => {
                    responder.json(400, json!({"error": error.to_string()}))
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        // The same numbers as /v1/metrics, shaped for a browser. Prometheus
        // text is for scrapers; a dashboard should not ship an exposition
        // parser just to draw one chart.
        ("GET", "/v1/metrics.json") => responder.json(200, metrics.as_json(engine)),
        ("GET", "/v1/metrics") => {
            // Prometheus text format. Engine stages first, then the HTTP
            // layer, so a reader sees the whole ingest path in one scrape.
            let mut rendered = String::with_capacity(2048);
            engine.metrics().render_prometheus(&mut rendered);
            metrics.render_prometheus(&mut rendered);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                rendered.len(),
                responder.connection_header()
            );
            responder.raw(&head, rendered.as_bytes())
        }
        ("GET", "/v1/stats") => match engine.stats() {
            Ok(stats) => responder.json(
                200,
                json!({
                    // These are physical storage records. Historical versions
                    // superseded by last-write-wins reads remain on disk until
                    // compaction, so calling them spans was misleading.
                    "record_count": stats.total_records,
                    "segment_count": stats.segment_count,
                    "bytes_on_disk": stats.disk_bytes,
                    "buffered_records": stats.buffered_records,
                    "persisted_records": stats.persisted_records,
                    "total_records": stats.total_records,
                    "durability": stats.durability.as_str(),
                    "wal_bytes": stats.wal_bytes,
                    "buffer_age_seconds": stats.buffer_age_seconds,
                }),
            ),
            Err(error) => responder.json(503, json!({"error": error.to_string()})),
        },
        ("GET", _) if path.starts_with("/v1/traces/") => {
            let id = percent_decode(&path[11..]);
            match engine.get_trace(&id) {
                Ok(spans) if spans.is_empty() => {
                    responder.json(404, json!({"error": "trace not found"}))
                }
                Ok(spans) => {
                    let annotations = engine.annotations(&id, None, None).unwrap_or_default();
                    responder.json(
                        200,
                        json!({
                            "trace_id": id,
                            "spans": serde_json::to_value(spans)
                                .unwrap_or_else(|_| Value::Array(Vec::new())),
                            "annotations": serde_json::to_value(annotations)
                                .unwrap_or_else(|_| Value::Array(Vec::new())),
                        }),
                    )
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/sessions") => {
            let (since, until, limit, group_by) = match analytics_query(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            if group_by.is_some() {
                return responder.json(
                    400,
                    json!({"error": "group_by is not a /v1/sessions parameter"}),
                );
            }
            match engine.sessions(
                since,
                until,
                limit.unwrap_or(100),
                traza::analytics::SessionOrder::Recent,
            ) {
                Ok(sessions) => responder.json(
                    200,
                    json!({"sessions": serde_json::to_value(sessions)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", _) if path.starts_with("/v1/sessions/") => {
            let id = percent_decode(&path[13..]);
            match engine.session(&id) {
                Ok(None) => responder.json(404, json!({"error": "session not found"})),
                Ok(Some(detail)) => responder.json(
                    200,
                    serde_json::to_value(detail).unwrap_or_else(|_| json!({})),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/annotations") => {
            let annotation: traza::annotations::Annotation =
                match serde_json::from_slice(&request.body) {
                    Ok(annotation) => annotation,
                    Err(error) => {
                        return responder.json(400, json!({"error": error.to_string()}));
                    }
                };
            let mut annotation = annotation;
            if annotation.timestamp_ns == 0 {
                annotation.timestamp_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
            }
            match engine.annotate(annotation) {
                Ok(()) => responder.json(200, json!({"recorded": true})),
                Err(traza::Error::InvalidSpan(reason)) => {
                    responder.json(400, json!({"error": reason}))
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/annotations") => {
            let mut trace_id = None;
            let mut span_id = None;
            let mut name = None;
            let mut source_prefix = None;
            let mut since_ns = None;
            let mut until_ns = None;
            let mut limit = None;
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                let value = percent_decode(value);
                match percent_decode(key).as_str() {
                    "trace_id" => trace_id = Some(value),
                    "span_id" => span_id = Some(value),
                    "name" => name = Some(value),
                    "source" => source_prefix = Some(value),
                    "since" | "since_ns" => match value.parse() {
                        Ok(parsed) => since_ns = Some(parsed),
                        Err(_) => return responder.json(400, json!({"error": "invalid since"})),
                    },
                    "until" | "until_ns" => match value.parse() {
                        Ok(parsed) => until_ns = Some(parsed),
                        Err(_) => return responder.json(400, json!({"error": "invalid until"})),
                    },
                    "limit" => match value.parse() {
                        Ok(parsed) => limit = Some(parsed),
                        Err(_) => return responder.json(400, json!({"error": "invalid limit"})),
                    },
                    other => {
                        return responder.json(
                            400,
                            json!({"error": format!("unknown query parameter: {other}")}),
                        )
                    }
                }
            }
            // `trace_id` used to be required, which made an eval run
            // unreadable: its scores exist per trace, but nobody wants them
            // one trace at a time. It is now one narrowing among several.
            let narrow = traza::annotations::AnnotationQuery {
                trace_id: trace_id.as_deref(),
                span_id: span_id.as_deref(),
                name: name.as_deref(),
                source_prefix: source_prefix.as_deref(),
                since_ns,
                until_ns,
                limit,
            };
            match engine.search_annotations(&narrow) {
                Ok(annotations) => responder.json(
                    200,
                    json!({"annotations": serde_json::to_value(annotations)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", _) if path.starts_with("/v1/payloads/") => {
            let reference = percent_decode(&path[13..]);
            match engine.payload(&reference) {
                Ok(Some(bytes)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                        bytes.len(),
                        responder.connection_header()
                    );
                    responder.raw(&head, &bytes)
                }
                Ok(None) => responder.json(404, json!({"error": "payload not found"})),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/export") => {
            let (filter, user_limit) = match filter_from_query(query) {
                Ok(mut filter) => {
                    // Exports default to unbounded, unlike interactive search.
                    let explicit = query.split('&').any(|pair| pair.starts_with("limit"));
                    let user_limit = if explicit { filter.limit } else { None };
                    filter.limit = None;
                    (filter, user_limit)
                }
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            // Chunked with trailers, terminated by the close. Keeping this
            // connection alive would need the client to agree to trailers
            // first; closing is correct and costs one connection per export.
            responder.must_close();
            stream_export(responder.stream, engine, filter, user_limit)
        }
        ("GET", "/v1/tail") => {
            let (filter, cursor, backfill) = match tail_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            // Server-sent events, terminated by the close. The connection is
            // the subscription: there is nothing to keep alive afterwards.
            responder.must_close();
            stream_tail(responder.stream, engine, &filter, cursor, backfill)
        }
        ("GET", "/v1/stats/llm") => {
            let (since, until, limit, group_by) = match analytics_query(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            let group_by =
                match group_by {
                    None => traza::analytics::LlmGroupBy::Model,
                    Some(name) => match traza::analytics::LlmGroupBy::parse(&name) {
                        Some(group) => group,
                        None => return responder.json(
                            400,
                            json!({"error": "group_by must be model|provider|service|session|day"}),
                        ),
                    },
                };
            match engine.llm_aggregate(group_by, since, until) {
                Ok(mut rows) => {
                    if let Some(limit) = limit {
                        rows.truncate(limit);
                    }
                    responder.json(
                        200,
                        json!({"rows": serde_json::to_value(rows)
                            .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                    )
                }
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/stats/series") => {
            let request = match series_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            let SeriesRequest {
                filter,
                buckets,
                window,
            } = request;
            let Some((since_ns, until_ns)) = window else {
                return responder.json(
                    400,
                    json!({"error": "since and until are required for a series"}),
                );
            };
            if until_ns <= since_ns {
                return responder.json(400, json!({"error": "until must be after since"}));
            }
            match engine.series(&filter, since_ns, until_ns, buckets.unwrap_or(24)) {
                Ok(series) => responder.json(
                    200,
                    serde_json::to_value(series).unwrap_or_else(|_| json!({})),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/stats/duration") => {
            let (filter, _) = match span_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            match engine.duration_histogram(&filter) {
                Ok(histogram) => responder.json(
                    200,
                    json!({
                        "count": histogram.count(),
                        "min_ns": histogram.min_ns(),
                        "max_ns": histogram.max_ns(),
                        "mean_ns": histogram.mean_ns(),
                        "p50_ns": histogram.percentile_ns(50.0),
                        "p75_ns": histogram.percentile_ns(75.0),
                        "p90_ns": histogram.percentile_ns(90.0),
                        "p95_ns": histogram.percentile_ns(95.0),
                        "p99_ns": histogram.percentile_ns(99.0),
                        // Only occupied buckets travel: a nanosecond-to-minute
                        // range occupies a few dozen of a thousand, and the
                        // zeros would be almost the entire payload.
                        "buckets": histogram
                            .occupied()
                            .into_iter()
                            .map(|(upper_ns, count)| json!({"upper_ns": upper_ns, "count": count}))
                            .collect::<Vec<_>>(),
                    }),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/stats/failures") => {
            let (filter, _) = match span_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            let limit = filter.limit.unwrap_or(100);
            match engine.failures(&filter, limit) {
                Ok(report) => responder.json(
                    200,
                    serde_json::to_value(report).unwrap_or_else(|_| json!({})),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/stats/slowest") => {
            let (filter, _) = match span_query_from(query) {
                Ok(parsed) => parsed,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            let limit = filter.limit.unwrap_or(10);
            match engine.slowest_spans(&filter, limit) {
                Ok(spans) => responder.json(
                    200,
                    json!({"spans": serde_json::to_value(spans)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
        _ => responder.json(404, json!({"error": "not found"})),
    }
}

/// A parsed series request.
///
/// The window is held separately from the filter because a series needs it as
/// a hard range rather than as an optional narrowing: without both bounds
/// there is nothing to divide into buckets.
struct SeriesRequest {
    filter: SpanFilter,
    buckets: Option<usize>,
    window: Option<(u64, u64)>,
}

/// Parses a series request. `buckets` is the one parameter
/// [`span_query_from`] does not know, so it is stripped before delegating.
fn series_query_from(raw_query: &str) -> Result<SeriesRequest, String> {
    let mut buckets = None;
    let mut kept = String::with_capacity(raw_query.len());
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key) == "buckets" {
            buckets = Some(
                percent_decode(value)
                    .parse()
                    .map_err(|_| "invalid buckets")?,
            );
            continue;
        }
        if !kept.is_empty() {
            kept.push('&');
        }
        kept.push_str(pair);
    }
    let (filter, _) = span_query_from(&kept)?;
    let window = match (filter.since_ns, filter.until_ns) {
        (Some(since), Some(until)) => Some((since, until)),
        _ => None,
    };
    Ok(SeriesRequest {
        filter,
        buckets,
        window,
    })
}

/// Streams an export as chunked NDJSON in constant-size pages.
///
/// The engine cursor carries the complete `(start, end, trace, span)` order,
/// so timestamp collisions never force a larger page or a prefix re-fetch.
/// Completion and emitted row count are explicit HTTP trailers: a storage
/// failure after `200 OK` is therefore distinguishable from a complete
/// dataset without adding control objects to the NDJSON body.
///
/// **Every page comes from one pinned snapshot.** Paging the live store meant
/// each page saw a different store: a span re-ingested behind the cursor came
/// back a second time, so an export could report `complete: true` over output
/// containing two versions of one primary key and a row count that matched no
/// state the store was ever in. `X-Traza-Export-Complete: true` now means the
/// stream is exactly the dataset that existed when the export began.
fn stream_export(
    stream: &mut TcpStream,
    engine: &Store,
    filter: SpanFilter,
    user_limit: Option<usize>,
) -> io::Result<()> {
    const EXPORT_PAGE: usize = 4_096;

    // Pin BEFORE the status line: pinning is the only part that can fail
    // before any bytes are committed, so its failure is still a clean 503.
    let view = match engine.snapshot() {
        Ok(view) => view,
        Err(error) => {
            let body = json!({"error": error.to_string()}).to_string();
            write!(
                stream,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
            return stream.flush();
        }
    };

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nTrailer: X-Traza-Export-Complete, X-Traza-Export-Count\r\nConnection: close\r\n\r\n"
    )?;
    let mut cursor: Option<SpanCursor> = None;
    let mut emitted = 0_usize;
    loop {
        let remaining = user_limit.map_or(EXPORT_PAGE, |limit| limit.saturating_sub(emitted));
        if remaining == 0 {
            return finish_export(stream, true, emitted);
        }
        let page_size = remaining.min(EXPORT_PAGE);
        let mut page_filter = filter.clone();
        page_filter.limit = Some(page_size);
        let page = match view.query_after(&page_filter, cursor.as_ref()) {
            Ok(page) => page,
            Err(error) => {
                eprintln!("export failed after {emitted} rows: {error}");
                return finish_export(stream, false, emitted);
            }
        };
        let fetched = page.len();
        for span in page {
            let mut line = match serde_json::to_vec(&span) {
                Ok(line) => line,
                Err(error) => {
                    eprintln!("export serialization failed after {emitted} rows: {error}");
                    return finish_export(stream, false, emitted);
                }
            };
            line.push(b'\n');
            write_chunk(stream, &line)?;
            cursor = Some(SpanCursor::from(&span));
            emitted += 1;
        }
        if fetched < page_size || user_limit.is_some_and(|limit| emitted >= limit) {
            return finish_export(stream, true, emitted);
        }
    }
}

/// How long a quiet tail waits before sending a comment frame.
///
/// Two jobs: it stops an intermediary from reaping a connection it believes is
/// dead, and it is the only way the server learns the client is gone. A tail
/// over a store that never receives a span would otherwise hold its thread
/// until the process ended, because a reader that never writes never discovers
/// a broken pipe.
const TAIL_HEARTBEAT: Duration = Duration::from_secs(15);

/// Spans of history a tail opens with when the client does not say.
///
/// Enough to fill the screen it is about to render. The backlog is free — it is
/// already in the ring — and without it the tail opens blank and stays blank
/// until something happens to be ingested, which reads as a broken page.
const DEFAULT_TAIL_BACKFILL: usize = 200;

/// Streams spans as they are admitted, as server-sent events.
///
/// This is the one surface ordered by ADMISSION rather than event time, and
/// that is the whole reason it exists. Paging a tail by `start_time_ns` — what
/// this replaced — drops any span that outlives one poll interval: the
/// watermark moves past a long operation while it is still running, and the
/// server then filters it out forever when it lands. Sequence numbers come
/// from the store's admission ring, so a span is delivered when it arrives
/// regardless of when it started.
///
/// Three frame types. `spans` carries matches and the position to resume from;
/// `gap` says the subscriber fell further behind than the ring retains and
/// must backfill by another means; a bare comment is the heartbeat.
fn stream_tail(
    stream: &mut TcpStream,
    engine: &Store,
    filter: &SpanFilter,
    cursor: Option<TailCursor>,
    backfill: usize,
) -> io::Result<()> {
    // `X-Accel-Buffering` is for reverse proxies that would otherwise hold
    // frames until a buffer fills, which for a stream that emits a few hundred
    // bytes at a time means indefinitely.
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Accel-Buffering: no\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let limit = filter.limit.unwrap_or(DEFAULT_TAIL_BACKFILL).max(1);
    let mut cursor = cursor;
    // Backlog is delivered once, on the opening read. Applying it again after
    // every heartbeat would re-send the same history on every quiet tick.
    let mut opening_backfill = backfill;

    loop {
        let read = match engine.tail_after(cursor, opening_backfill, limit, filter, TAIL_HEARTBEAT)
        {
            Ok(read) => read,
            // The filter was validated before the status line went out, so
            // reaching this means a predicate the tail cannot honour got past
            // the parser. Ending the stream is the only honest response left
            // once the response has already begun.
            Err(error) => {
                eprintln!("tail refused its filter mid-stream: {error}");
                return stream.flush();
            }
        };
        opening_backfill = 0;
        let frame = match read {
            TailRead::Batch {
                spans,
                cursor: next,
            } => {
                let settled = Some(next) == cursor;
                cursor = Some(next);
                if spans.is_empty() && settled {
                    // Nothing arrived within the heartbeat window. A comment
                    // frame is valid SSE and the client ignores it, which is
                    // exactly what a keepalive should be.
                    ": tick\n\n".to_owned()
                } else {
                    // `Arc<Span>` is serialized through a reference rather than
                    // directly: serde only implements `Serialize` for `Arc`
                    // under its `rc` feature, which this crate does not enable.
                    let rows: Vec<&Span> = spans.iter().map(|span| span.as_ref()).collect();
                    let payload = json!({
                        "spans": serde_json::to_value(&rows)
                            .unwrap_or_else(|_| Value::Array(Vec::new())),
                        "cursor": next.to_token(),
                    });
                    format!("event: spans\ndata: {payload}\n\n")
                }
            }
            TailRead::Gap { missed } => {
                // A gap is a discontinuity, not a position to resume from. The
                // subscriber restarts at the live edge with a fresh backlog,
                // and everything it held before the break is void.
                //
                // Resuming from the ring's floor instead — the first design —
                // replayed every retained entry while the client was fetching
                // an overlapping window by event time, producing duplicates
                // with nothing to deduplicate them, and claiming to have
                // recovered an interval that no query can actually address.
                cursor = None;
                // The subscriber's own backfill, not a forced default. A client
                // that asked for zero history wants zero history after a gap
                // too — overriding it sent four retained spans to a caller that
                // had explicitly requested none.
                opening_backfill = backfill;
                let payload = json!({"missed": missed});
                format!("event: gap\ndata: {payload}\n\n")
            }
        };
        // A failed write is how a disconnect is detected — there is no other
        // signal, and it is what ends this thread.
        write_chunk(stream, frame.as_bytes())?;
        stream.flush()?;
    }
}

/// Query parser for the tail: the span filter, plus `cursor` and `backfill`.
fn tail_query_from(raw_query: &str) -> Result<(SpanFilter, Option<TailCursor>, usize), String> {
    let mut cursor = None;
    let mut backfill = DEFAULT_TAIL_BACKFILL;
    let mut rest: Vec<&str> = Vec::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match percent_decode(key).as_str() {
            "cursor" => {
                let token = percent_decode(value);
                cursor = Some(
                    TailCursor::parse(&token).ok_or("cursor is not a token this server issued")?,
                );
            }
            "backfill" => {
                backfill = percent_decode(value)
                    .parse()
                    .map_err(|_| "invalid backfill")?;
            }
            // Rejected rather than ignored. A tail is ordered by admission, so
            // an event-time bound cannot be honoured here — and silently
            // dropping it would answer a different question than the one asked,
            // which is precisely the failure this endpoint was built to end.
            "since" | "since_ns" | "until" | "until_ns" => {
                return Err(format!(
                    "{key} does not apply to a tail: it streams in admission order, \
                     not event-time order. Use /v1/spans for a time window."
                ));
            }
            _ => rest.push(pair),
        }
    }
    // The remaining parameters are the ordinary span filter, so every
    // predicate the search screen offers works here too, unchanged.
    let (filter, _) = span_query_from(&rest.join("&"))?;
    Ok((filter, cursor, backfill))
}

fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    write!(stream, "{:X}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")
}

fn finish_export(stream: &mut TcpStream, complete: bool, emitted: usize) -> io::Result<()> {
    write!(
        stream,
        "0\r\nX-Traza-Export-Complete: {}\r\nX-Traza-Export-Count: {emitted}\r\n\r\n",
        if complete { "true" } else { "false" }
    )?;
    stream.flush()
}

/// Query parser for the analytics endpoints: `since`/`until` (ns), `limit`,
/// `group_by`. Unknown parameters are rejected like everywhere else.
#[allow(clippy::type_complexity)]
fn analytics_query(
    raw_query: &str,
) -> Result<(Option<u64>, Option<u64>, Option<usize>, Option<String>), String> {
    let mut since = None;
    let mut until = None;
    let mut limit = None;
    let mut group_by = None;
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        let value = percent_decode(value);
        match key.as_str() {
            "since" | "since_ns" => {
                since = Some(value.parse().map_err(|_| "invalid since")?);
            }
            "until" | "until_ns" => {
                until = Some(value.parse().map_err(|_| "invalid until")?);
            }
            "limit" => {
                limit = Some(value.parse().map_err(|_| "invalid limit")?);
            }
            "group_by" => group_by = Some(value),
            other => return Err(format!("unknown query parameter: {other}")),
        }
    }
    Ok((since, until, limit, group_by))
}

fn filter_from_query(raw_query: &str) -> Result<SpanFilter, String> {
    span_query_from(raw_query).map(|(filter, _)| filter)
}

/// Parses a span search: the filter plus an optional pagination cursor.
///
/// The cursor is not part of [`SpanFilter`] because it is not a predicate —
/// it says where the previous page stopped, and the same filter answers
/// differently on each page by design.
fn span_query_from(raw_query: &str) -> Result<(SpanFilter, Option<traza::SpanCursor>), String> {
    // The README's contract: default limit 100, applied after filtering.
    let mut filter = SpanFilter {
        limit: Some(100),
        ..SpanFilter::default()
    };
    let mut cursor = None;
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        let value = percent_decode(value);
        if let Some(attribute) = key.strip_prefix("attr.") {
            // The value is parsed as JSON when it looks like JSON, but the
            // engine compares scalars by text as well as by type, so
            // `attr.code=200` matches whether the span stored 200 or "200".
            // It used to match only the number, and a store full of
            // stringified codes answered every such query with nothing.
            let parsed = serde_json::from_str::<Value>(&value)
                .unwrap_or_else(|_| Value::String(value.clone()));
            filter.attributes.push((attribute.to_owned(), parsed));
            continue;
        }
        if let Some(attribute) = key.strip_prefix("not_attr.") {
            let parsed = serde_json::from_str::<Value>(&value)
                .unwrap_or_else(|_| Value::String(value.clone()));
            filter
                .excluded_attributes
                .push((attribute.to_owned(), parsed));
            continue;
        }
        if let Some(attribute) = key.strip_prefix("min_attr.") {
            let bound: f64 = value
                .parse()
                .map_err(|_| format!("min_attr.{attribute} must be a number"))?;
            filter.min_attributes.push((attribute.to_owned(), bound));
            continue;
        }
        if let Some(attribute) = key.strip_prefix("max_attr.") {
            let bound: f64 = value
                .parse()
                .map_err(|_| format!("max_attr.{attribute} must be a number"))?;
            filter.max_attributes.push((attribute.to_owned(), bound));
            continue;
        }
        match key.as_str() {
            "service" => filter.service = Some(value),
            "name" => filter.name = Some(value),
            // The span's own status field, not an attribute. `attr.status=`
            // reads an attribute most instrumentation never writes, so it
            // looked like this filter existed while answering nothing.
            "status" => filter.status = Some(value),
            "not_status" => filter.excluded_statuses.push(value),
            // Word search over the span's text. Not substring, not a phrase —
            // see `SpanFilter::content`.
            "content" | "q" => filter.content = Some(value),
            "cursor" => {
                cursor = Some(
                    traza::SpanCursor::from_token(&value)
                        .ok_or("cursor is not a token this server issued")?,
                );
            }
            // Unions every recognized session key, so a mixed-convention
            // session (some spans session.id, some gen_ai.conversation.id)
            // returns whole — unlike attr.session.id, which sees one key.
            "session" => filter.session = Some(value),
            "min_duration_ms" => {
                let ms: u64 = value.parse().map_err(|_| "invalid min_duration_ms")?;
                filter.min_duration_ns = Some(ms.saturating_mul(1_000_000));
            }
            "min_duration_ns" => {
                filter.min_duration_ns =
                    Some(value.parse().map_err(|_| "invalid min_duration_ns")?);
            }
            "since" | "since_ns" => {
                filter.since_ns = Some(value.parse().map_err(|_| "invalid since")?);
            }
            "until" | "until_ns" => {
                filter.until_ns = Some(value.parse().map_err(|_| "invalid until")?);
            }
            "max_duration_ms" => {
                let ms: u64 = value.parse().map_err(|_| "invalid max_duration_ms")?;
                filter.max_duration_ns = Some(ms.saturating_mul(1_000_000));
            }
            "max_duration_ns" => {
                filter.max_duration_ns =
                    Some(value.parse().map_err(|_| "invalid max_duration_ns")?);
            }
            "sort" => {
                filter.sort = Some(traza::SpanSort::parse(&value).ok_or(
                    "sort must be one of duration|-duration|start|-start \
                     (or the long forms duration_asc, duration_desc, start_asc, start_desc)",
                )?);
            }
            "limit" => {
                filter.limit = Some(value.parse().map_err(|_| "invalid limit")?);
            }
            other => return Err(format!("unknown query parameter: {other}")),
        }
    }
    Ok((filter, cursor))
}

/// Serves the MCP endpoint: the Streamable HTTP transport's server half.
///
/// Deliberately not an SSE stream. Every tool here is request/response, none
/// of them needs the server to speak first, and a stream would be state this
/// surface has decided not to keep. `GET` and `DELETE` therefore answer `405`,
/// which the specification defines as "this endpoint offers no such stream"
/// and "sessions cannot be terminated here" respectively.
fn serve_mcp(
    responder: &mut Responder<'_>,
    request: &Request,
    engine: &Store,
    options: &McpOptions,
    access: traza::mcp::Access,
) -> io::Result<()> {
    if !options.enabled {
        return responder.json(
            404,
            json!({
                "error": "the MCP endpoint is not enabled",
                "next": "restart traza-server with --mcp (add --mcp-annotations to let an \
                         rw token record annotations)",
            }),
        );
    }
    // DNS rebinding defence. A browser attaches `Origin` and a page on an
    // attacker's domain cannot forge it, so refusing an origin the operator
    // did not name stops a remote site driving this server through the user's
    // browser. Native MCP clients send no `Origin` at all and are unaffected.
    if let Some(origin) = &request.origin {
        if !origin_allowed(origin, &options.allowed_origins) {
            return responder.json(
                403,
                json!({
                    "error": "origin not allowed",
                    "next": "loopback origins are allowed by default; name any other with \
                             --mcp-allowed-origin ORIGIN",
                }),
            );
        }
    }
    // The specification requires a 400 for a version this server does not
    // serve, rather than silently answering in a dialect the client cannot
    // read.
    if let Some(version) = &request.mcp_protocol_version {
        if !traza::mcp::SUPPORTED_VERSIONS.contains(&version.as_str()) {
            return responder.json(
                400,
                json!({
                    "error": format!("unsupported MCP-Protocol-Version: {version}"),
                    "supported": traza::mcp::SUPPORTED_VERSIONS,
                }),
            );
        }
    }
    if request.method != "POST" {
        return responder.json(
            405,
            json!({
                "error": "this MCP endpoint accepts POST only",
                "next": "POST one JSON-RPC message per request; there is no SSE stream and \
                         no session to delete",
            }),
        );
    }
    let message: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(error) => {
            return responder.json(
                400,
                traza::mcp::error_response(Value::Null, -32700, &format!("parse error: {error}")),
            );
        }
    };
    if message.is_array() {
        return responder.json(
            400,
            traza::mcp::error_response(
                Value::Null,
                -32600,
                "JSON-RPC batching was removed from MCP; send one message per request",
            ),
        );
    }
    let server = traza::mcp::Server::new(engine, options.limits, options.annotations);
    let context = traza::mcp::Context {
        access,
        now_ns: traza::mcp::unix_nanos_now(),
    };
    match server.handle(&message, context) {
        Some(response) => responder.json(200, response),
        // A notification or a response: accepted, and by the transport's rule
        // answered with no body at all.
        None => responder.accepted(),
    }
}

/// Whether a browser `Origin` may drive this endpoint.
///
/// **The origin is never compared against anything else the request carries.**
/// An earlier version accepted any `Origin` whose authority equalled the
/// request's `Host`, which is precisely what a DNS-rebinding request supplies:
/// the attacker owns the name, so the browser sends *their* host in both
/// headers and the comparison succeeds. Only two things may permit an origin:
///
/// - it is a loopback page, which an attacker's site can never be — the
///   browser stamps the origin of the page, and a page served from
///   `evil.example` has that origin however its DNS resolves;
/// - the operator named it with `--mcp-allowed-origin`.
fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    // "null" is what a sandboxed iframe or a `file://` page sends. It is
    // same-origin with nothing, and matches no allowlist entry.
    if origin.is_empty() || origin == "null" {
        return false;
    }
    let origin = origin.trim().trim_end_matches('/');
    if allowed.iter().any(|entry| entry == origin) {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    // A loopback *origin* — not a loopback destination. The scheme is checked
    // too, so `javascript://localhost` and friends cannot pass as one.
    matches!(scheme, "http" | "https")
        && matches!(authority_host(authority), "localhost" | "127.0.0.1" | "::1")
}

/// The host part of an authority, with an IPv6 literal unwrapped from its
/// brackets so `[::1]:8080` compares as `::1`.
fn authority_host(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    authority.split(':').next().unwrap_or(authority)
}

struct Request {
    method: String,
    target: String,
    content_type: String,
    /// `Origin:` and `MCP-Protocol-Version:` — carried because the MCP
    /// endpoint has header-level rules the REST routes do not.
    origin: Option<String>,
    mcp_protocol_version: Option<String>,
    body: Vec<u8>,
}

/// A parsed request head: everything except the body, which is read only
/// AFTER the auth gate passes — an unauthenticated client must not be able
/// to make the server buffer a 64 MiB body just by declaring one.
struct RequestHead {
    method: String,
    target: String,
    /// Request-line version, uppercased. Decides the keep-alive default.
    version: String,
    authorization: Option<String>,
    content_type: String,
    /// Lowercased `Connection:` header value, empty when absent.
    connection: String,
    /// `Origin:` as sent. Needed by the MCP endpoint, which must refuse a
    /// browser origin the operator did not name, to defeat DNS rebinding.
    ///
    /// Deliberately NOT accompanied by `Host`. Validating one against the
    /// other is the bug this field exists to avoid: a rebinding request
    /// supplies both, so they agree exactly when the check matters most.
    origin: Option<String>,
    /// `MCP-Protocol-Version:` as sent. An unsupported value is a `400`.
    mcp_protocol_version: Option<String>,
    content_length: usize,
    header_end: usize,
}

impl RequestHead {
    /// Whether the client is willing to reuse this connection.
    ///
    /// HTTP/1.1 persists unless it says `close`; HTTP/1.0 closes unless it
    /// says `keep-alive`. Compared token-wise because the header is a list —
    /// `Connection: keep-alive, Upgrade` is one value, not two headers.
    fn wants_keep_alive(&self) -> bool {
        let tokens = || self.connection.split(',').map(str::trim);
        if self.version == "HTTP/1.1" {
            !tokens().any(|token| token == "close")
        } else {
            tokens().any(|token| token == "keep-alive")
        }
    }
}

/// One client connection, with the bytes it has read but not yet consumed.
///
/// Keep-alive is why this holds a buffer at all: a read for one request's body
/// can pull in the head of the next, and a fresh per-request buffer would
/// silently drop those bytes. Everything consumed is drained from the front,
/// so what remains is always the start of the next request.
struct Connection {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Connection {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(8192),
        }
    }

    /// Borrows the socket for one response, declaring up front whether the
    /// connection survives it.
    fn responder(&mut self, keep_alive: bool) -> Responder<'_> {
        Responder {
            stream: &mut self.stream,
            keep_alive,
            status: None,
        }
    }

    /// Reads the next request head.
    ///
    /// `Ok(None)` means the peer closed cleanly at a request boundary — the
    /// ordinary end of a keep-alive connection, not an error.
    fn read_head(&mut self) -> io::Result<Option<RequestHead>> {
        let mut chunk = [0_u8; 8192];
        let header_end = loop {
            // Check BEFORE reading: a pipelined request may already be sitting
            // in the buffer, and blocking on a read the client will never
            // satisfy would hang it until the socket timeout.
            if let Some(position) = find_bytes(&self.buffer, b"\r\n\r\n") {
                break position + 4;
            }
            if self.buffer.len() > MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request headers too large",
                ));
            }
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete request",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        };
        parse_head(&self.buffer, header_end).map(Some)
    }

    /// Reads `head`'s body and consumes the whole request from the buffer.
    fn read_body(&mut self, head: &RequestHead) -> io::Result<Vec<u8>> {
        let needed = head.header_end + head.content_length;
        let mut chunk = [0_u8; 8192];
        while self.buffer.len() < needed {
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete body",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
        let body = self.buffer[head.header_end..needed].to_vec();
        // Whatever is left belongs to the next request.
        self.buffer.drain(..needed);
        Ok(body)
    }
}

fn parse_head(bytes: &[u8], header_end: usize) -> io::Result<RequestHead> {
    // The in-loop check only bounds growth while SEARCHING; a terminator
    // arriving in the final chunk can still complete an oversized header,
    // so the found header must be re-checked against the cap.
    if header_end > MAX_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request headers too large",
        ));
    }
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    // Absent version means HTTP/0.9, which has no keep-alive; defaulting to
    // 1.0 makes the persistence decision below opt-in rather than assumed.
    let version = request_line
        .next()
        .unwrap_or("HTTP/1.0")
        .to_ascii_uppercase();
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }
    let mut authorization = None;
    let mut content_type = String::new();
    let mut connection = String::new();
    let mut origin = None;
    let mut mcp_protocol_version = None;
    let mut header_lines: Vec<&str> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
            if name.eq_ignore_ascii_case("content-type") {
                content_type = value.trim().to_ascii_lowercase();
            }
            if name.eq_ignore_ascii_case("connection") {
                connection = value.trim().to_ascii_lowercase();
            }
            if name.eq_ignore_ascii_case("origin") {
                origin = Some(value.trim().to_ascii_lowercase());
            }
            if name.eq_ignore_ascii_case("mcp-protocol-version") {
                mcp_protocol_version = Some(value.trim().to_owned());
            }
        }
        header_lines.push(line);
    }
    // Once connections persist, request framing is security-relevant: anything
    // ambiguous about where this body ends lets a crafted request be split
    // into two, the second of which the server would attribute to the client's
    // NEXT request. Both ambiguities are refused rather than resolved.
    if header_lines
        .iter()
        .filter_map(|line| line.split_once(':'))
        .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transfer-encoded request bodies are not accepted; send Content-Length",
        ));
    }
    let mut lengths = header_lines
        .iter()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim());
    let first = lengths.next();
    if lengths.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate content-length",
        ));
    }
    let content_length = first
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid content-length"))?
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }
    Ok(RequestHead {
        method,
        target,
        version,
        authorization,
        content_type,
        connection,
        origin,
        mcp_protocol_version,
        content_length,
        header_end,
    })
}

/// Writes one response on a connection, stamping the `Connection:` header that
/// matches what the connection is actually going to do next.
///
/// The flag has to live here rather than at each call site because getting it
/// wrong is a correctness bug, not a performance one: announcing keep-alive
/// and then closing (or the reverse) desynchronizes the client's framing.
struct Responder<'a> {
    stream: &'a mut TcpStream,
    /// Whether this connection will read another request after this response.
    keep_alive: bool,
    /// Status of the response actually sent, for per-class accounting. A
    /// request that never reached a responder leaves this `None` and is not
    /// counted — an uncounted request beats one filed under a status it
    /// never had.
    status: Option<u16>,
}

impl Responder<'_> {
    fn connection_header(&self) -> &'static str {
        if self.keep_alive {
            "keep-alive"
        } else {
            "close"
        }
    }

    /// Refuses to serve anything further on this connection.
    ///
    /// Used wherever the request framing is no longer trustworthy — a body we
    /// declined to read, a malformed head — because leaving unread bytes in
    /// the socket would let them be parsed as the NEXT request, which is
    /// request smuggling.
    fn must_close(&mut self) {
        self.keep_alive = false;
    }

    /// Sends a JSON body. Head and body go out in one write: at ingest rates
    /// the extra syscall per response is pure overhead.
    fn json(&mut self, status: u16, body: Value) -> io::Result<()> {
        self.status = Some(status);
        let encoded = serde_json::to_vec(&body)?;
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Error",
        };
        self.raw(
            &format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                encoded.len(),
                self.connection_header(),
            ),
            &encoded,
        )
    }

    /// Acknowledges a request that has no reply — an MCP notification. The
    /// transport specifies 202 *with no body*, so this is not `json`.
    fn accepted(&mut self) -> io::Result<()> {
        self.status = Some(202);
        self.raw(
            &format!(
                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: {}\r\n\r\n",
                self.connection_header(),
            ),
            &[],
        )
    }

    /// Writes a static UI file. `no-store` keeps a rebuilt dashboard from being
    /// shadowed by a cached copy; `nosniff` stops a browser from
    /// reinterpreting a served asset as something more dangerous than its
    /// declared type.
    fn file(&mut self, file: &traza::ui::UiFile) -> io::Result<()> {
        self.raw(
            &format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                file.content_type,
                file.bytes.len(),
                self.connection_header(),
            ),
            &file.bytes,
        )
    }

    /// Sends a caller-built head followed by `body`, in a single write.
    fn raw(&mut self, head: &str, body: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(head.len() + body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                output.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        output.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Options {
        let args: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        parse_args(&args)
            .expect("parses")
            .expect("not a help invocation")
    }

    #[test]
    fn no_profile_is_the_balanced_defaults() {
        let options = parse(&[]);
        assert_eq!(options.profile, Profile::Balanced);
        assert_eq!(options.config.flush_spans, Config::default().flush_spans);
        assert_eq!(options.config.wal_commit_window, None);
    }

    #[test]
    fn a_profile_sets_its_group_of_knobs() {
        let throughput = parse(&["--profile", "throughput"]);
        assert_eq!(
            throughput.config.flush_spans,
            Profile::Throughput.flush_spans()
        );
        assert_eq!(
            throughput.config.wal_commit_window,
            Profile::Throughput.wal_commit_window()
        );

        let latency = parse(&["--profile", "latency"]);
        assert_eq!(latency.config.flush_spans, Profile::Latency.flush_spans());
        assert_eq!(latency.config.wal_commit_window, None);
    }

    /// The precedence rule, checked in BOTH argument orders. Order-dependence
    /// is the whole failure mode here: a parser that applied the profile as it
    /// saw it would silently discard a `--flush-spans` that came first.
    #[test]
    fn an_explicit_flag_beats_the_profile_in_either_order() {
        for argv in [
            vec!["--profile", "throughput", "--flush-spans", "77"],
            vec!["--flush-spans", "77", "--profile", "throughput"],
        ] {
            let options = parse(&argv);
            assert_eq!(
                options.config.flush_spans, 77,
                "explicit --flush-spans lost to the profile in {argv:?}"
            );
            // The knobs the flag did NOT name still come from the profile.
            assert_eq!(
                options.config.wal_commit_window,
                Profile::Throughput.wal_commit_window(),
                "overriding one knob dropped the rest of the profile in {argv:?}"
            );
        }

        for argv in [
            vec!["--profile", "latency", "--wal-commit-window-us", "900"],
            vec!["--wal-commit-window-us", "900", "--profile", "latency"],
        ] {
            let options = parse(&argv);
            assert_eq!(
                options.config.wal_commit_window,
                Some(Duration::from_micros(900)),
                "explicit --wal-commit-window-us lost to the profile in {argv:?}"
            );
            assert_eq!(options.config.flush_spans, Profile::Latency.flush_spans());
        }
    }

    /// `0` means "no window", which is a different statement from "unset" and
    /// has to survive a profile that asked for one.
    #[test]
    fn an_explicit_zero_window_overrides_a_profile_that_wants_one() {
        assert!(Profile::Throughput.wal_commit_window().is_some());
        for argv in [
            vec!["--profile", "throughput", "--wal-commit-window-us", "0"],
            vec!["--wal-commit-window-us", "0", "--profile", "throughput"],
        ] {
            assert_eq!(
                parse(&argv).config.wal_commit_window,
                None,
                "explicit 0 did not turn the profile's window off in {argv:?}"
            );
        }
    }

    /// No profile may weaken the acknowledgement contract. The enum makes a
    /// lossy profile unrepresentable; this pins the resolved behaviour so a
    /// later "throughput means buffered" shortcut fails here.
    #[test]
    fn no_profile_changes_durability() {
        for name in ["throughput", "balanced", "latency"] {
            assert_eq!(
                parse(&["--profile", name]).config.durability,
                Durability::Wal,
                "profile {name} moved durability off the default"
            );
            // And an explicitly chosen mode survives a profile untouched.
            assert_eq!(
                parse(&["--profile", name, "--durability", "flushed"])
                    .config
                    .durability,
                Durability::Flushed
            );
        }
        // Buffered stays reachable only by asking for it by name.
        assert_eq!(
            parse(&["--durability", "buffered"]).config.durability,
            Durability::Buffered
        );
    }

    /// Compaction is a read-path choice, so a write-path profile must leave it
    /// alone — including the "off" an operator asked for.
    #[test]
    fn no_profile_changes_compaction() {
        for name in ["throughput", "balanced", "latency"] {
            let options = parse(&["--profile", name]);
            assert_eq!(options.config.compaction, Some(CompactionConfig::default()));
            assert!(options.compaction_enabled);
            assert_eq!(
                parse(&["--profile", name, "--compaction-fanout", "0"])
                    .config
                    .compaction,
                None
            );
        }
    }

    #[test]
    fn the_last_profile_wins_and_an_unknown_one_is_refused() {
        assert_eq!(
            parse(&["--profile", "throughput", "--profile", "latency"]).profile,
            Profile::Latency
        );
        assert!(parse_args(&["--profile".to_owned(), "fast".to_owned()]).is_err());
        assert!(parse_args(&["--profile".to_owned()]).is_err());
    }

    /// Flags outside the profile's remit keep working under a profile, and
    /// keep their own defaults when it does not name them.
    #[test]
    fn non_profile_flags_are_untouched_by_a_profile() {
        let options = parse(&[
            "--profile",
            "throughput",
            "--max-connections",
            "32",
            "--payload-threshold-bytes",
            "0",
            "--ttl-seconds",
            "60",
        ]);
        assert_eq!(options.max_connections, 32);
        assert_eq!(options.config.payload_threshold, None);
        assert_eq!(options.config.ttl_seconds, Some(60));

        let latency = parse(&["--profile", "latency"]);
        assert_eq!(latency.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(latency.config.payload_threshold, Some(256 * 1024));
    }

    #[test]
    fn help_parses_to_nothing_to_run() {
        assert!(parse_args(&["--help".to_owned()])
            .expect("help is not an error")
            .is_none());
    }

    #[test]
    fn version_parses_to_nothing_to_run() {
        assert!(parse_args(&["--version".to_owned()])
            .expect("version is not an error")
            .is_none());
        assert!(parse_args(&["-V".to_owned()])
            .expect("version is not an error")
            .is_none());
    }
}
