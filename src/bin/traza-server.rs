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
//! - `GET /v1/spans?service=&name=&min_duration_ns=&since_ns=&until_ns=&limit=`
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
//!   This is the one route that always closes its connection: it is chunked
//!   with trailers and has no declared length.
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
use traza::{CompactionConfig, Config, Durability, Span, SpanCursor, SpanFilter, Store};

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

/// Server-side request instrumentation, alongside the engine's own.
#[derive(Debug, Default)]
struct ServerMetrics {
    /// Requests served to completion.
    requests: traza::metrics::Counter,
    /// End-to-end handling time, from parsed head to written response.
    request_latency: traza::metrics::Latency,
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

impl ServerMetrics {
    fn render_prometheus(&self, into: &mut String) {
        use std::fmt::Write as _;
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
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("traza-server: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("./data");
    let mut host = String::from("127.0.0.1");
    let mut port = 8080_u16;
    let mut ttl_seconds = None;
    let mut flush_spans = 10_000_usize;
    let mut payload_threshold_bytes = 256 * 1024_usize;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut allow_unauthenticated_non_loopback = false;
    let mut ui_dir: Option<PathBuf> = None;
    let mut durability = Durability::default();
    let mut compaction = CompactionConfig::default();
    let mut compaction_enabled = true;
    let mut wal_commit_window_us = 0_u64;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(args.get(i).ok_or("--data-dir requires a value")?);
            }
            "--host" => {
                i += 1;
                host = args.get(i).ok_or("--host requires a value")?.clone();
            }
            "--port" => {
                i += 1;
                port = args.get(i).ok_or("--port requires a value")?.parse()?;
            }
            "--ttl-seconds" => {
                i += 1;
                ttl_seconds = Some(
                    args.get(i)
                        .ok_or("--ttl-seconds requires a value")?
                        .parse()?,
                );
            }
            "--payload-threshold-bytes" => {
                i += 1;
                payload_threshold_bytes = args
                    .get(i)
                    .ok_or("--payload-threshold-bytes requires a value")?
                    .parse::<usize>()?;
            }
            "--flush-spans" => {
                i += 1;
                flush_spans = args
                    .get(i)
                    .ok_or("--flush-spans requires a value")?
                    .parse::<usize>()?
                    .max(1);
            }
            "--max-connections" => {
                i += 1;
                max_connections = args
                    .get(i)
                    .ok_or("--max-connections requires a value")?
                    .parse::<usize>()?
                    .max(1);
            }
            "--compaction-fanout" => {
                i += 1;
                let value: usize = args
                    .get(i)
                    .ok_or("--compaction-fanout requires a value")?
                    .parse()?;
                // 0 or 1 cannot merge anything; treat as "off" rather than
                // silently looping.
                compaction_enabled = value >= 2;
                compaction.fanout = value.max(2);
            }
            "--compaction-max-segment-bytes" => {
                i += 1;
                compaction.max_segment_bytes = args
                    .get(i)
                    .ok_or("--compaction-max-segment-bytes requires a value")?
                    .parse()?;
            }
            "--wal-commit-window-us" => {
                i += 1;
                wal_commit_window_us = args
                    .get(i)
                    .ok_or("--wal-commit-window-us requires a value")?
                    .parse()?;
            }
            "--durability" => {
                i += 1;
                let name = args.get(i).ok_or("--durability requires a value")?;
                durability =
                    Durability::parse(name).ok_or("--durability must be buffered|wal|flushed")?;
            }
            "--ui-dir" => {
                i += 1;
                ui_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--ui-dir requires a value")?,
                ));
            }
            "--allow-unauthenticated-non-loopback" => {
                allow_unauthenticated_non_loopback = true;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: traza-server --data-dir DIR --port PORT [--host ADDR] [--ttl-seconds N] [--flush-spans N] [--max-connections N (default 1024)] [--payload-threshold-bytes N (0 disables)] [--durability buffered|wal|flushed (default wal)] [--wal-commit-window-us N (delay each fsync so more acks share it; 0 = off)] [--compaction-fanout N (0 disables; default 4)] [--compaction-max-segment-bytes N] [--ui-dir DIR (built dashboard; default: TRAZA_UI_DIR, beside the binary, then ./ui/dist)] [--allow-unauthenticated-non-loopback]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }

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
    let engine = Arc::new(Store::open(
        &data_dir,
        Config {
            flush_spans,
            ttl_seconds,
            // 0 disables offloading.
            payload_threshold: (payload_threshold_bytes > 0).then_some(payload_threshold_bytes),
            durability,
            compaction: compaction_enabled.then_some(compaction),
            wal_commit_window: (wal_commit_window_us > 0)
                .then(|| Duration::from_micros(wal_commit_window_us)),
        },
    )?);

    // TTL enforcement and segment compaction live in the engine; the server
    // only schedules them. Compaction ticks far more often than TTL: it is
    // what keeps filtered-search latency flat as segments accumulate, and it
    // is a no-op when no run qualifies.
    if ttl_seconds.is_some() || compaction_enabled {
        let maintainer = Arc::clone(&engine);
        let ttl_enabled = ttl_seconds.is_some();
        thread::Builder::new()
            .name("traza-maintenance".into())
            .spawn(move || {
                let mut ticks = 0u64;
                loop {
                    thread::sleep(std::time::Duration::from_secs(5));
                    ticks += 1;
                    if let Err(error) = maintainer.compact_segments() {
                        eprintln!("segment compaction failed: {error}");
                    }
                    // TTL keeps its documented one-minute cadence.
                    if ttl_enabled && ticks % 12 == 0 {
                        if let Err(error) = maintainer.compact_expired() {
                            eprintln!("expiry compaction failed: {error}");
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

    // The dashboard is served from disk (ui/ `npm run build` output), never
    // compiled in. A missing build is not fatal: the API runs, and the UI
    // routes explain how to produce it.
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
        let spawned = thread::Builder::new()
            .name("traza-http".into())
            .spawn(move || {
                let _ = handle_connection(
                    stream,
                    &connection_engine,
                    &connection_auth,
                    &connection_ui,
                    &connection_metrics,
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
        if let Some(config) = auth {
            if let Err(failure) = config.authorize(head.authorization.as_deref(), &head.method) {
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
            body,
        };
        let mut responder = connection.responder(keep_alive);
        let result = serve_request(&mut responder, request, engine, metrics);
        let persist = responder.keep_alive;
        result?;
        metrics
            .request_latency
            .record(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        metrics.requests.increment();
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
) -> io::Result<()> {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
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
        ("GET", "/v1/spans") => {
            let filter = match filter_from_query(query) {
                Ok(filter) => filter,
                Err(error) => return responder.json(400, json!({"error": error})),
            };
            match engine.query(&filter) {
                Ok(spans) => responder.json(
                    200,
                    serde_json::to_value(spans).unwrap_or_else(|_| Value::Array(Vec::new())),
                ),
                Err(error) => responder.json(503, json!({"error": error.to_string()})),
            }
        }
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
            match engine.sessions(since, until, limit.unwrap_or(100)) {
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
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                match percent_decode(key).as_str() {
                    "trace_id" => trace_id = Some(percent_decode(value)),
                    "span_id" => span_id = Some(percent_decode(value)),
                    "name" => name = Some(percent_decode(value)),
                    other => {
                        return responder.json(
                            400,
                            json!({"error": format!("unknown query parameter: {other}")}),
                        )
                    }
                }
            }
            let Some(trace_id) = trace_id else {
                return responder.json(400, json!({"error": "trace_id is required"}));
            };
            match engine.annotations(&trace_id, span_id.as_deref(), name.as_deref()) {
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
        _ => responder.json(404, json!({"error": "not found"})),
    }
}

/// Streams an export as chunked NDJSON in constant-size pages.
///
/// The engine cursor carries the complete `(start, end, trace, span)` order,
/// so timestamp collisions never force a larger page or a prefix re-fetch.
/// Completion and emitted row count are explicit HTTP trailers: a storage
/// failure after `200 OK` is therefore distinguishable from a complete
/// dataset without adding control objects to the NDJSON body.
fn stream_export(
    stream: &mut TcpStream,
    engine: &Store,
    filter: SpanFilter,
    user_limit: Option<usize>,
) -> io::Result<()> {
    const EXPORT_PAGE: usize = 4_096;

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
        let page = match engine.query_after(&page_filter, cursor.as_ref()) {
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
    // The README's contract: default limit 100, applied after filtering.
    let mut filter = SpanFilter {
        limit: Some(100),
        ..SpanFilter::default()
    };
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        let value = percent_decode(value);
        if let Some(attribute) = key.strip_prefix("attr.") {
            // Bare values match string attributes; JSON literals match typed
            // values — the documented attr.KEY semantics.
            let parsed = serde_json::from_str::<Value>(&value)
                .unwrap_or_else(|_| Value::String(value.clone()));
            filter.attributes.push((attribute.to_owned(), parsed));
            continue;
        }
        match key.as_str() {
            "service" => filter.service = Some(value),
            "name" => filter.name = Some(value),
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
            "limit" => {
                filter.limit = Some(value.parse().map_err(|_| "invalid limit")?);
            }
            other => return Err(format!("unknown query parameter: {other}")),
        }
    }
    Ok(filter)
}

struct Request {
    method: String,
    target: String,
    content_type: String,
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
        let encoded = serde_json::to_vec(&body)?;
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
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
