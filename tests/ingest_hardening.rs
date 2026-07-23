//! Ingest hardening: adversarial wire inputs and engine-boundary invariants.
//!
//! Every probe here is a class the ordinary suites never exercised: hostile
//! HTTP framing (lying content-length, oversized headers, silent peers),
//! degenerate ids at the library boundary, and query-parameter extremes.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::{Config, Error, Span, Store};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, tokens: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .env_remove("TRAZA_TOKENS")
            // Short socket deadline so silent-peer probes finish quickly.
            .env("TRAZA_SOCKET_TIMEOUT_MS", "500")
            .stderr(Stdio::piped());
        if let Some(tokens) = tokens {
            command.env("TRAZA_TOKENS", tokens);
        }
        let mut child = command.spawn().expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = std::io::BufReader::new(stderr);
        let port = {
            use std::io::BufRead;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("stderr read");
                if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                    break rest.trim().parse::<u16>().expect("port parses");
                }
            }
        };
        std::thread::spawn(move || {
            use std::io::BufRead;
            for _ in reader.lines() {}
        });
        Self { child, port }
    }

    fn connect(&self) -> TcpStream {
        let mut attempt = 0;
        loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(stream) => break stream,
                Err(_) if attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect: {error}"),
            }
        }
    }

    fn raw(&self, request: &str) -> String {
        let mut stream = self.connect();
        stream.write_all(request.as_bytes()).expect("writes");
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = self.connect();
        let body_len = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body");
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads");
        let text = String::from_utf8_lossy(&response);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let payload = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        (status, payload)
    }

    fn kill(self) {
        drop(self);
    }
}

// A panicking test must never leak its server child: cargo waits on the
// child's inherited pipes, hanging the whole test binary long after the
// failure it should be reporting.
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-harden-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span_json(trace_id: &str, span_id: &str, name: &str, start: u64, end: u64) -> Value {
    json!({"trace_id": trace_id, "span_id": span_id, "name": name, "service": "svc",
           "start_time_ns": start, "end_time_ns": end})
}

#[test]
fn library_rejects_empty_ids_atomically() {
    let dir = test_dir("lib-ids");
    let store = Store::open(&dir, Config::default()).expect("opens");

    let empty_span = Span {
        span_id: String::new(),
        ..sample_span("t", "s")
    };
    assert!(matches!(
        store.ingest(empty_span.clone()),
        Err(Error::InvalidSpan("span_id is empty"))
    ));
    let empty_trace = Span {
        trace_id: String::new(),
        ..sample_span("t", "s")
    };
    assert!(matches!(
        store.ingest(empty_trace),
        Err(Error::InvalidSpan("trace_id is empty"))
    ));

    // Batch validation is atomic: the valid sibling must not be stored.
    let result = store.ingest_batch(vec![sample_span("t-atomic", "ok"), empty_span]);
    assert!(matches!(result, Err(Error::InvalidSpan(_))));
    assert!(
        store.get_trace("t-atomic").expect("query").is_empty(),
        "a rejected batch must store nothing"
    );
}

fn sample_span(trace_id: &str, span_id: &str) -> Span {
    serde_json::from_value(span_json(trace_id, span_id, "op", 1_000, 2_000)).expect("span")
}

#[test]
fn hostile_ids_round_trip_through_flush_and_reopen() {
    // Ids are opaque bytes to the engine: NUL prefixes (the attribute-index
    // reserved namespace), unicode, and whitespace must all round-trip
    // without colliding with internal index keys.
    let dir = test_dir("hostile-ids");
    let hostile = ["\u{0}service", "id with spaces", "emoji-🦀-id", "a&b=c?d"];
    {
        let store = Store::open(&dir, Config::default()).expect("opens");
        for (index, id) in hostile.iter().enumerate() {
            store
                .ingest(sample_span(id, &format!("s{index}")))
                .expect("ingests");
        }
        store.flush().expect("flushes");
    }
    let store = Store::open(&dir, Config::default()).expect("reopens");
    for id in hostile {
        let spans = store.get_trace(id).expect("queries");
        assert_eq!(spans.len(), 1, "trace {id:?} must survive flush+reopen");
        assert_eq!(spans[0].trace_id, id);
    }
}

#[test]
fn end_before_start_is_stored_and_never_panics_queries() {
    let dir = test_dir("time-warp");
    let server = Server::spawn(&dir, None);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json("t-warp", "s1", "op", 2_000, 1_000)])),
    );
    assert_eq!(status, 200, "inverted timestamps are the client's business");
    // Duration filtering over the inverted span must not underflow.
    let (status, body) = server.request("GET", "/v1/spans?min_duration_ns=1", None);
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(0),
        "inverted span has saturated zero duration"
    );
    server.kill();
}

#[test]
fn query_parameter_extremes_are_rejected_or_bounded() {
    let dir = test_dir("params");
    let server = Server::spawn(&dir, None);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json("t-q", "s1", "op", 1_000, 2_000)])),
    );
    assert_eq!(status, 200);

    // limit=0 is honored (empty page), huge limits are fine, junk is 400.
    let (status, body) = server.request("GET", "/v1/spans?limit=0", None);
    assert_eq!(status, 200);
    assert_eq!(body.as_array().map(Vec::len), Some(0));
    let (status, _) = server.request("GET", "/v1/spans?limit=18446744073709551615", None);
    assert_eq!(status, 200);
    for bad in [
        "/v1/spans?limit=-1",
        "/v1/spans?limit=nope",
        "/v1/spans?since=later",
        "/v1/spans?min_duration_ms=-5",
        "/v1/spans?surprise=1",
    ] {
        let (status, _) = server.request("GET", bad, None);
        assert_eq!(status, 400, "{bad} must be rejected");
    }
    // Weird-but-legal attribute keys must not panic the filter path.
    let (status, _) = server.request("GET", "/v1/spans?attr.=bare", None);
    assert_eq!(status, 200);
    let (status, _) = server.request("GET", "/v1/spans?attr.%00hidden=x", None);
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn lying_content_length_gets_a_400_not_a_hang() {
    let dir = test_dir("liar");
    let server = Server::spawn(&dir, None);

    // Declared body never arrives; the peer half-closes. The server must
    // answer 400 (incomplete body), not wait forever.
    let started = Instant::now();
    let response = server.raw(
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n[",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response:.60}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "lying content-length must not park the connection"
    );

    // A declared body over the cap is refused from the head alone.
    let response = server.raw(
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999999\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response:.60}");
    // Server still healthy afterwards.
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json("t-alive", "s1", "op", 1, 2)])),
    );
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn oversized_headers_are_rejected() {
    let dir = test_dir("bighead");
    let server = Server::spawn(&dir, None);
    let padding = "x".repeat(70 * 1024);
    let response = server.raw(&format!(
        "GET /v1/stats HTTP/1.1\r\nHost: x\r\nX-Pad: {padding}\r\nConnection: close\r\n\r\n"
    ));
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response:.60}");
    server.kill();
}

#[test]
fn silent_connections_are_released() {
    let dir = test_dir("silent");
    let server = Server::spawn(&dir, None);

    // A peer that connects and never speaks must be dropped by the socket
    // deadline (500ms in this harness), freeing the worker thread.
    let mut idle = server.connect();
    let started = Instant::now();
    let mut sink = Vec::new();
    // Whether the server answers 400 or just closes, read_to_end must
    // complete once the socket deadline fires.
    let _ = idle.read_to_end(&mut sink);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "silent connection must be released by the socket deadline"
    );

    // And the server still serves real requests afterwards.
    let (status, _) = server.request("GET", "/v1/stats", None);
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn unauthenticated_requests_are_refused_before_the_body() {
    let dir = test_dir("preauth");
    let server = Server::spawn(&dir, Some("rw:secret-token"));

    // No credentials, one-megabyte declared body, body never sent: the 401
    // must arrive from the head alone — pre-fix the server buffered the
    // declared body first, letting unauthenticated peers burn its memory.
    let started = Instant::now();
    let mut stream = server.connect();
    stream
        .write_all(
            b"POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
        )
        .expect("writes head");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 401"),
        "401 must not wait for the body: {text:.60}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(2_000),
        "auth verdict must precede the body read"
    );
    server.kill();
}

#[test]
fn malformed_otlp_shapes_are_rejected_not_crashed() {
    let dir = test_dir("otlp-shapes");
    let server = Server::spawn(&dir, None);
    for body in [
        json!({"resourceSpans": 42}),
        json!({"resourceSpans": [null]}),
        json!({"resourceSpans": [{"scopeSpans": [{"spans": [{}]}]}]}),
        json!({"resourceSpans": [{"scopeSpans": [{"spans": [{"traceId": "", "spanId": ""}]}]}]}),
        json!([1, 2, 3]),
    ] {
        let (status, _) = server.request("POST", "/v1/traces", Some(&body));
        assert_eq!(status, 400, "shape {body} must be a 400");
    }
    // Healthy afterwards.
    let (status, _) = server.request("GET", "/v1/stats", None);
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn duplicate_key_within_one_batch_is_last_write_wins() {
    // The documented primary-key semantic, pinned so it never silently
    // changes: same (trace_id, span_id) twice in one batch upserts in order.
    let dir = test_dir("dup-key");
    let server = Server::spawn(&dir, None);
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            span_json("t-dup", "s1", "first", 1_000, 2_000),
            span_json("t-dup", "s1", "second", 1_000, 2_000),
        ])),
    );
    assert_eq!(status, 200);
    assert_eq!(body["accepted"], 2, "both inputs are accepted");
    let (status, body) = server.request("GET", "/v1/traces/t-dup", None);
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 1, "one stored span for one primary key");
    assert_eq!(spans[0]["name"], "second", "last write wins");
    server.kill();
}
