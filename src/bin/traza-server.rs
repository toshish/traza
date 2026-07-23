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
//! - `POST /v1/flush` forces buffered spans into a durable segment.
//! - `POST /v1/traces` accepts an OTLP/HTTP JSON ExportTraceServiceRequest.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};
use traza::{Config, Span, SpanFilter, Store};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const HTTP_QUEUE_DEPTH: usize = 256;

fn main() {
    if let Err(error) = run() {
        eprintln!("traza-server: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("./data");
    let mut host = String::from("0.0.0.0");
    let mut port = 8080_u16;
    let mut ttl_seconds = None;
    let mut flush_spans = 10_000_usize;
    let mut workers = thread::available_parallelism()
        .map_or(4, usize::from)
        .max(4);
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
            "--help" | "-h" => {
                println!(
                    "Usage: traza-server --data-dir DIR --port PORT [--host ADDR] [--ttl-seconds N] [--flush-spans N] [--workers N]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }

    // Bearer auth from TRAZA_TOKENS; unset = open (development default). A
    // set-but-invalid value refuses startup — running open when the operator
    // tried to configure auth would be the worst failure mode.
    let auth = Arc::new(
        traza::auth::AuthConfig::from_env()
            .map_err(|error| format!("auth configuration: {error}"))?,
    );
    let engine = Arc::new(Store::open(
        &data_dir,
        Config {
            flush_spans,
            ttl_seconds,
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

    let (http_tx, http_rx) = mpsc::sync_channel::<TcpStream>(HTTP_QUEUE_DEPTH);
    let http_rx = Arc::new(Mutex::new(http_rx));
    for number in 0..workers {
        let rx = Arc::clone(&http_rx);
        let worker_engine = Arc::clone(&engine);
        let worker_auth = Arc::clone(&auth);
        thread::Builder::new()
            .name(format!("http-{number}"))
            .spawn(move || loop {
                let stream = match rx.lock().expect("HTTP receiver poisoned").recv() {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                let _ = handle_connection(stream, &worker_engine, &worker_auth);
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

fn handle_connection(
    mut stream: TcpStream,
    engine: &Store,
    auth: &Option<traza::auth::AuthConfig>,
) -> io::Result<()> {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => return respond(&mut stream, 400, json!({"error": error.to_string()})),
    };
    if let Some(config) = auth {
        if let Err(failure) = config.authorize(request.authorization.as_deref(), &request.method) {
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
                        if span.trace_id.is_empty() {
                            return respond(
                                &mut stream,
                                400,
                                json!({"error": format!("span {index}: trace_id is empty")}),
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
            // OTLP/HTTP JSON: an ExportTraceServiceRequest mapped onto the
            // span model (docs: leg-2 spec / README).
            let value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => {
                    return respond(&mut stream, 400, json!({"error": error.to_string()}));
                }
            };
            let spans = match traza::otlp::spans_from_request(&value) {
                Ok(spans) => spans,
                Err(error) => {
                    return respond(&mut stream, 400, json!({"error": error.to_string()}));
                }
            };
            match engine.ingest_batch(spans) {
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
                    // Documented keys first — span count, segment count,
                    // bytes on disk — then the engine's finer-grained view.
                    "span_count": stats.total_spans,
                    "segment_count": stats.segment_count,
                    "bytes_on_disk": stats.disk_bytes,
                    "buffered_spans": stats.buffered_spans,
                    "persisted_spans": stats.persisted_spans,
                    "total_spans": stats.total_spans,
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
                Ok(spans) => respond(
                    &mut stream,
                    200,
                    json!({
                        "trace_id": id,
                        "spans": serde_json::to_value(spans)
                            .unwrap_or_else(|_| Value::Array(Vec::new())),
                    }),
                ),
                Err(error) => respond(&mut stream, 503, json!({"error": error.to_string()})),
            }
        }
        _ => respond(&mut stream, 404, json!({"error": "not found"})),
    }
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
    authorization: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
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
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
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
    let mut header_lines: Vec<&str> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
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
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete body",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(Request {
        method,
        target,
        authorization,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
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
