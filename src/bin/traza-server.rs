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
//!   responds `{"accepted": N}`.
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
//! - `GET /` and `GET /dashboard` serve the built dashboard from `--ui-dir`
//!   (default `./ui/dist`, produced by `ui/`'s `npm run build`). The page is
//!   read from disk, never compiled in, so building the server needs no Node
//!   toolchain and a rebuilt UI is picked up without restarting. The shell is
//!   served before the auth gate — it carries no data, and its `/v1` calls
//!   stay gated — and the routes 404 with build instructions when absent.

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use traza::{Config, Span, SpanCursor, SpanFilter, Store};

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
const HTTP_QUEUE_DEPTH: usize = 256;

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
    let mut workers = thread::available_parallelism()
        .map_or(4, usize::from)
        .max(4);
    let mut allow_unauthenticated_non_loopback = false;
    let mut ui_dir = PathBuf::from("./ui/dist");
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
            "--workers" => {
                i += 1;
                workers = args
                    .get(i)
                    .ok_or("--workers requires a value")?
                    .parse::<usize>()?
                    .max(1);
            }
            "--ui-dir" => {
                i += 1;
                ui_dir = PathBuf::from(args.get(i).ok_or("--ui-dir requires a value")?);
            }
            "--allow-unauthenticated-non-loopback" => {
                allow_unauthenticated_non_loopback = true;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: traza-server --data-dir DIR --port PORT [--host ADDR] [--ttl-seconds N] [--flush-spans N] [--workers N] [--payload-threshold-bytes N (0 disables)] [--ui-dir DIR (built dashboard; default ./ui/dist)] [--allow-unauthenticated-non-loopback]"
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
    let engine = Arc::new(Store::open(
        &data_dir,
        Config {
            flush_spans,
            ttl_seconds,
            // 0 disables offloading.
            payload_threshold: (payload_threshold_bytes > 0).then_some(payload_threshold_bytes),
        },
    )?);

    // TTL enforcement lives in the engine; the server only schedules it.
    if ttl_seconds.is_some() {
        let compactor = Arc::clone(&engine);
        thread::Builder::new()
            .name("traza-compactor".into())
            .spawn(move || loop {
                thread::sleep(std::time::Duration::from_secs(60));
                if let Err(error) = compactor.compact_expired() {
                    eprintln!("compaction failed: {error}");
                }
            })?;
    }

    // --port 0 binds an ephemeral port; the actual port is announced on
    // stderr so process-level tests can discover it.
    let listener = TcpListener::bind((host.as_str(), port))?;
    let actual_port = listener.local_addr()?.port();
    eprintln!("traza-server listening on {host}:{actual_port}");

    // The dashboard is served from disk (ui/ `npm run build` output), never
    // compiled in. A missing build is not fatal: the API runs, and the UI
    // routes explain how to produce it.
    let ui = Arc::new(traza::ui::UiRoot::new(&ui_dir));
    if ui.is_available() {
        eprintln!("traza-server serving dashboard from {}", ui_dir.display());
    } else {
        eprintln!(
            "traza-server: no dashboard at {} (build it with: cd ui && npm ci && npm run build)",
            ui_dir.display()
        );
    }

    let (http_tx, http_rx) = mpsc::sync_channel::<TcpStream>(HTTP_QUEUE_DEPTH);
    let http_rx = Arc::new(Mutex::new(http_rx));
    for number in 0..workers {
        let rx = Arc::clone(&http_rx);
        let worker_engine = Arc::clone(&engine);
        let worker_auth = Arc::clone(&auth);
        let worker_ui = Arc::clone(&ui);
        thread::Builder::new()
            .name(format!("http-{number}"))
            .spawn(move || loop {
                let stream = match rx.lock().expect("HTTP receiver poisoned").recv() {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                let _ = handle_connection(stream, &worker_engine, &worker_auth, &worker_ui);
            })?;
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = http_tx.send(stream);
            }
            Err(error) => eprintln!("accept failed: {error}"),
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

fn handle_connection(
    mut stream: TcpStream,
    engine: &Store,
    auth: &Option<traza::auth::AuthConfig>,
    ui: &traza::ui::UiRoot,
) -> io::Result<()> {
    // A silent or dribbling peer must not park this worker thread forever.
    let timeout = socket_timeout();
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(error) => return respond(&mut stream, 400, json!({"error": error.to_string()})),
    };
    // The dashboard SHELL is served before the auth gate: the page must load
    // in a browser without credentials, while every /v1 call it makes below
    // stays gated (the page attaches the bearer token itself). Static assets
    // carry no stored data, so this leaks nothing.
    if head.method == "GET" {
        let path = percent_decode(
            head.target
                .split_once('?')
                .map_or(head.target.as_str(), |(path, _)| path),
        );
        if let Some(file) = ui.resolve(&path) {
            return respond_file(&mut stream, &file);
        }
        if matches!(path.as_str(), "/" | "/dashboard" | "/dashboard/") {
            return respond(
                &mut stream,
                404,
                json!({
                    "error": "no dashboard build found",
                    "next": format!(
                        "build it with: cd ui && npm ci && npm run build (serving {})",
                        ui.directory().display()
                    ),
                }),
            );
        }
    }
    // Auth verdicts need only the head: rejecting BEFORE the body read means
    // an unauthenticated client cannot make this server buffer 64 MiB.
    if let Some(config) = auth {
        if let Err(failure) = config.authorize(head.authorization.as_deref(), &head.method) {
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
            write!(
                stream,
                "HTTP/1.1 {} {reason}\r\n{challenge}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                failure.status(),
                body.len(),
            )?;
            return Ok(());
        }
    }
    let request = match read_body(&mut stream, head) {
        Ok(request) => request,
        Err(error) => return respond(&mut stream, 400, json!({"error": error.to_string()})),
    };
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    match (request.method.as_str(), path) {
        ("POST", "/v1/spans") => {
            let value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => {
                    return respond(&mut stream, 400, json!({"error": error.to_string()}));
                }
            };
            let raw_spans = if let Some(array) = value.as_array() {
                array.clone()
            } else if let Some(array) = value.get("spans").and_then(Value::as_array) {
                array.clone()
            } else {
                return respond(
                    &mut stream,
                    400,
                    json!({"error": "body must be an array or {spans: [...]}"}),
                );
            };
            let mut spans = Vec::with_capacity(raw_spans.len());
            for (index, raw) in raw_spans.into_iter().enumerate() {
                match serde_json::from_value::<Span>(raw) {
                    Ok(span) => {
                        // Both halves of the (trace_id, span_id) primary key
                        // must be non-empty: an empty span_id would make every
                        // such span in a trace one colliding key, silently
                        // upserted over each other while the response counts
                        // them all as accepted.
                        if span.trace_id.is_empty() {
                            return respond(
                                &mut stream,
                                400,
                                json!({"error": format!("span {index}: trace_id is empty")}),
                            );
                        }
                        if span.span_id.is_empty() {
                            return respond(
                                &mut stream,
                                400,
                                json!({"error": format!("span {index}: span_id is empty")}),
                            );
                        }
                        spans.push(span);
                    }
                    Err(error) => {
                        return respond(
                            &mut stream,
                            400,
                            json!({"error": format!("span {index}: {error}")}),
                        );
                    }
                }
            }
            let accepted = spans.len();
            match engine.ingest_batch(spans) {
                Ok(()) => respond(&mut stream, 200, json!({"accepted": accepted})),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/traces") => {
            // OTLP/HTTP: an ExportTraceServiceRequest, binary protobuf or
            // JSON by Content-Type. The protobuf decoder lowers to the JSON
            // shape, so both encodings share one mapping (docs: README).
            let is_protobuf = request.content_type.starts_with("application/x-protobuf");
            let value: Value = if is_protobuf {
                match traza::otlp_pb::traces_request_to_json(&request.body) {
                    Ok(value) => value,
                    Err(error) => {
                        return respond(&mut stream, 400, json!({"error": error.to_string()}));
                    }
                }
            } else {
                match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(error) => {
                        return respond(&mut stream, 400, json!({"error": error.to_string()}));
                    }
                }
            };
            let spans = match traza::otlp::spans_from_request(&value) {
                Ok(spans) => spans,
                Err(error) => {
                    return respond(&mut stream, 400, json!({"error": error.to_string()}));
                }
            };
            match engine.ingest_batch(spans) {
                Ok(()) if is_protobuf => {
                    // An empty ExportTraceServiceResponse is zero protobuf
                    // bytes; protobuf clients expect the matching media type.
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                }
                Ok(()) => respond(&mut stream, 200, json!({"partialSuccess": {}})),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/flush") => match engine.flush() {
            Ok(()) => respond(&mut stream, 200, json!({"flushed": true})),
            Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
        },
        ("GET", "/v1/spans") => {
            let filter = match filter_from_query(query) {
                Ok(filter) => filter,
                Err(error) => return respond(&mut stream, 400, json!({"error": error})),
            };
            match engine.query(&filter) {
                Ok(spans) => respond(
                    &mut stream,
                    200,
                    serde_json::to_value(spans).unwrap_or_else(|_| Value::Array(Vec::new())),
                ),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/stats") => match engine.stats() {
            Ok(stats) => respond(
                &mut stream,
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
                }),
            ),
            Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
        },
        ("GET", _) if path.starts_with("/v1/traces/") => {
            let id = percent_decode(&path[11..]);
            match engine.get_trace(&id) {
                Ok(spans) if spans.is_empty() => {
                    respond(&mut stream, 404, json!({"error": "trace not found"}))
                }
                Ok(spans) => {
                    let annotations = engine.annotations(&id, None, None).unwrap_or_default();
                    respond(
                        &mut stream,
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
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("GET", "/v1/sessions") => {
            let (since, until, limit, group_by) = match analytics_query(query) {
                Ok(parsed) => parsed,
                Err(error) => return respond(&mut stream, 400, json!({"error": error})),
            };
            if group_by.is_some() {
                return respond(
                    &mut stream,
                    400,
                    json!({"error": "group_by is not a /v1/sessions parameter"}),
                );
            }
            match engine.sessions(since, until, limit.unwrap_or(100)) {
                Ok(sessions) => respond(
                    &mut stream,
                    200,
                    json!({"sessions": serde_json::to_value(sessions)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                ),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("GET", _) if path.starts_with("/v1/sessions/") => {
            let id = percent_decode(&path[13..]);
            match engine.session(&id) {
                Ok(None) => respond(&mut stream, 404, json!({"error": "session not found"})),
                Ok(Some(detail)) => respond(
                    &mut stream,
                    200,
                    serde_json::to_value(detail).unwrap_or_else(|_| json!({})),
                ),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("POST", "/v1/annotations") => {
            let annotation: traza::annotations::Annotation =
                match serde_json::from_slice(&request.body) {
                    Ok(annotation) => annotation,
                    Err(error) => {
                        return respond(&mut stream, 400, json!({"error": error.to_string()}));
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
                Ok(()) => respond(&mut stream, 200, json!({"recorded": true})),
                Err(traza::Error::InvalidSpan(reason)) => {
                    respond(&mut stream, 400, json!({"error": reason}))
                }
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
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
                        return respond(
                            &mut stream,
                            400,
                            json!({"error": format!("unknown query parameter: {other}")}),
                        )
                    }
                }
            }
            let Some(trace_id) = trace_id else {
                return respond(&mut stream, 400, json!({"error": "trace_id is required"}));
            };
            match engine.annotations(&trace_id, span_id.as_deref(), name.as_deref()) {
                Ok(annotations) => respond(
                    &mut stream,
                    200,
                    json!({"annotations": serde_json::to_value(annotations)
                        .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                ),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        ("GET", _) if path.starts_with("/v1/payloads/") => {
            let reference = percent_decode(&path[13..]);
            match engine.payload(&reference) {
                Ok(Some(bytes)) => {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )?;
                    stream.write_all(&bytes)
                }
                Ok(None) => respond(&mut stream, 404, json!({"error": "payload not found"})),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
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
                Err(error) => return respond(&mut stream, 400, json!({"error": error})),
            };
            stream_export(&mut stream, engine, filter, user_limit)
        }
        ("GET", "/v1/stats/llm") => {
            let (since, until, limit, group_by) = match analytics_query(query) {
                Ok(parsed) => parsed,
                Err(error) => return respond(&mut stream, 400, json!({"error": error})),
            };
            let group_by = match group_by {
                None => traza::analytics::LlmGroupBy::Model,
                Some(name) => match traza::analytics::LlmGroupBy::parse(&name) {
                    Some(group) => group,
                    None => {
                        return respond(
                            &mut stream,
                            400,
                            json!({"error": "group_by must be model|provider|service|session|day"}),
                        )
                    }
                },
            };
            match engine.llm_aggregate(group_by, since, until) {
                Ok(mut rows) => {
                    if let Some(limit) = limit {
                        rows.truncate(limit);
                    }
                    respond(
                        &mut stream,
                        200,
                        json!({"rows": serde_json::to_value(rows)
                            .unwrap_or_else(|_| Value::Array(Vec::new()))}),
                    )
                }
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        _ => respond(&mut stream, 404, json!({"error": "not found"})),
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
    authorization: Option<String>,
    content_type: String,
    content_length: usize,
    /// Bytes read so far (headers plus any body prefix that arrived with them).
    buffered: Vec<u8>,
    header_end: usize,
}

fn read_head(stream: &mut TcpStream) -> io::Result<RequestHead> {
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 8192];
    let header_end;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete request",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    }
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
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }
    let mut authorization = None;
    let mut content_type = String::new();
    let mut header_lines: Vec<&str> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
            if name.eq_ignore_ascii_case("content-type") {
                content_type = value.trim().to_ascii_lowercase();
            }
        }
        header_lines.push(line);
    }
    let content_length = header_lines
        .iter()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
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
        authorization,
        content_type,
        content_length,
        buffered: bytes,
        header_end,
    })
}

fn read_body(stream: &mut TcpStream, head: RequestHead) -> io::Result<Request> {
    let RequestHead {
        method,
        target,
        content_type,
        content_length,
        mut buffered,
        header_end,
        ..
    } = head;
    let mut buffer = [0_u8; 8192];
    while buffered.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete body",
            ));
        }
        buffered.extend_from_slice(&buffer[..read]);
    }
    Ok(Request {
        method,
        target,
        content_type,
        body: buffered[header_end..header_end + content_length].to_vec(),
    })
}

/// Writes a static UI file. `no-store` keeps a rebuilt dashboard from being
/// shadowed by a cached copy; `nosniff` stops a browser from reinterpreting a
/// served asset as something more dangerous than its declared type.
fn respond_file(stream: &mut TcpStream, file: &traza::ui::UiFile) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        file.content_type,
        file.bytes.len()
    )?;
    stream.write_all(&file.bytes)?;
    stream.flush()
}

fn respond(stream: &mut TcpStream, status: u16, body: Value) -> io::Result<()> {
    let encoded = serde_json::to_vec(&body)?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    )?;
    stream.write_all(&encoded)
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
