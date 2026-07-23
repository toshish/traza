//! Server-on-engine milestone integration coverage.
//!
//! Every test spawns the real `traza-server` binary and drives it over its
//! actual HTTP wire contract. The engine-authority tests additionally open the
//! server's data directory with `traza::Store` directly and compare what the
//! engine holds against what the server accepted — proving the server has no
//! private store of its own.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::{Config, Store};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// Spawns the real binary on an ephemeral port and waits for its
    /// listening announcement.
    fn spawn(data_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_traza-server"))
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--flush-spans")
            .arg("100000")
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines
                .next()
                .expect("server exited before announcing its port")
                .expect("stderr read failed");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        // Keep draining stderr so the child never blocks on a full pipe.
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("body encodes"));
        let mut stream = connect_with_retry(self.port);
        let body_len = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("request writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body writes");
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response reads");
        let text = String::from_utf8_lossy(&response);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .expect("status parses");
        let payload = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .map(|body| serde_json::from_str(body).expect("response body is JSON"))
            .unwrap_or(Value::Null);
        (status, payload)
    }

    /// SIGKILL — deliberately ungraceful, so restart tests also prove the
    /// engine's stale-lock recovery instead of relying on clean shutdown.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect_with_retry(port: u16) -> TcpStream {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return stream;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server on port {port} never accepted a connection");
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-server-on-engine-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test dir creates");
    dir
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn span_json(trace_id: &str, span_id: &str, service: &str, name: &str, start: u64) -> Value {
    json!({
        "trace_id": trace_id,
        "span_id": span_id,
        "parent_span_id": null,
        "name": name,
        "start_time_ns": start,
        "end_time_ns": start + 1_000,
        "status": "ok",
        "service": service,
        "attributes": {},
        "events": [],
    })
}

#[test]
fn server_write_round_trip_uses_engine() {
    let dir = test_dir("write");
    let trace_id = format!("trace-write-{}", nonce());
    let server = Server::spawn(&dir);

    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            span_json(&trace_id, "s1", "checkout", "charge-card", 100),
            span_json(&trace_id, "s2", "checkout", "emit-receipt", 200),
        ])),
    );
    assert_eq!(status, 200, "ingest failed: {body}");
    assert_eq!(body["accepted"], 2);

    let (status, body) = server.request("GET", &format!("/v1/traces/{trace_id}"), None);
    assert_eq!(status, 200, "trace read failed: {body}");
    assert_eq!(body["trace_id"], trace_id.as_str());
    let spans = body["spans"].as_array().expect("spans array");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0]["span_id"], "s1");
    assert_eq!(spans[0]["name"], "charge-card");
    assert_eq!(spans[1]["span_id"], "s2");

    // Engine authority: after a flush, the engine — opened directly, with the
    // server gone — must hold exactly what the server accepted.
    let (status, _) = server.request("POST", "/v1/flush", None);
    assert_eq!(status, 200);
    server.kill();
    let engine = Store::open(&dir, Config::default()).expect("engine opens after server exit");
    let spans = engine.get_trace(&trace_id).expect("engine read");
    assert_eq!(spans.len(), 2, "engine does not hold the server's writes");
    assert_eq!(spans[0].span_id, "s1");
    assert_eq!(spans[0].service, "checkout");
    assert_eq!(spans[1].span_id, "s2");
}

#[test]
fn server_read_query_round_trip_uses_engine() {
    let dir = test_dir("query");
    let marker = nonce();
    let server = Server::spawn(&dir);

    let service_a = format!("svc-a-{marker}");
    let service_b = format!("svc-b-{marker}");
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!({"spans": [
            span_json("trace-q1", "a1", &service_a, "lookup", 100),
            span_json("trace-q1", "a2", &service_a, "store", 300),
            span_json("trace-q2", "b1", &service_b, "lookup", 200),
        ]})),
    );
    assert_eq!(status, 200);

    let (status, body) = server.request("GET", &format!("/v1/spans?service={service_a}"), None);
    assert_eq!(status, 200, "query failed: {body}");
    let spans = body.as_array().expect("query returns an array");
    assert_eq!(spans.len(), 2, "service filter must isolate svc-a: {body}");
    assert!(spans
        .iter()
        .all(|span| span["service"] == service_a.as_str()));
    assert_eq!(
        spans[0]["span_id"], "a1",
        "spans must come back start-ordered"
    );

    let (status, body) = server.request(
        "GET",
        &format!("/v1/spans?service={service_b}&name=lookup"),
        None,
    );
    assert_eq!(status, 200);
    let spans = body.as_array().expect("array");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["span_id"], "b1");

    let (status, body) = server.request("GET", "/v1/spans?service=absent-service", None);
    assert_eq!(status, 200);
    assert_eq!(body.as_array().map(Vec::len), Some(0));
    server.kill();
}

#[test]
fn server_reopen_preserves_spans() {
    let dir = test_dir("reopen");
    let trace_id = format!("trace-reopen-{}", nonce());

    let server = Server::spawn(&dir);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json(
            &trace_id,
            "r1",
            "billing",
            "persist-me",
            100
        )])),
    );
    assert_eq!(status, 200);
    let (status, _) = server.request("POST", "/v1/flush", None);
    assert_eq!(status, 200);
    server.kill();

    // A killed process leaves its LOCK behind; reopening here also proves the
    // engine reclaims a stale lock instead of wedging the store forever.
    let server = Server::spawn(&dir);
    let (status, body) = server.request("GET", &format!("/v1/traces/{trace_id}"), None);
    assert_eq!(status, 200, "reopened server lost the trace: {body}");
    let spans = body["spans"].as_array().expect("spans array");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["span_id"], "r1");
    assert_eq!(spans[0]["name"], "persist-me");
    server.kill();
}

#[test]
fn server_invalid_write_preserves_error_contract() {
    let dir = test_dir("invalid");
    let server = Server::spawn(&dir);

    // Malformed JSON is rejected with a diagnostic.
    let mut stream = connect_with_retry(server.port);
    write!(
        stream,
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot-json!"
    )
    .expect("writes");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("reads");
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // A structurally valid body that is not a span list is rejected.
    let (status, body) = server.request("POST", "/v1/spans", Some(&json!({"nope": true})));
    assert_eq!(status, 400);
    assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));

    // An empty trace id is named in the rejection.
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json("", "s1", "svc", "op", 1)])),
    );
    assert_eq!(status, 400);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("trace_id is empty")),
        "diagnostic must identify the empty trace id: {body}"
    );

    // An empty span id is rejected the same way: (trace_id, span_id) is the
    // primary key, so two distinct spans with empty span_id are one colliding
    // key — accepting them upserted the second over the first while the
    // response counted both (found live: accepted:2, one span stored).
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            span_json("t-empty-sid", "", "svc", "first", 1),
            span_json("t-empty-sid", "", "svc", "second", 2),
        ])),
    );
    assert_eq!(status, 400);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("span 0: span_id is empty")),
        "diagnostic must identify the empty span id: {body}"
    );
    // The rejection is atomic: nothing from the batch was stored.
    let (status, _) = server.request("GET", "/v1/traces/t-empty-sid", None);
    assert_eq!(status, 404);

    // The rejections must not have corrupted the server: a valid write and
    // read still succeed afterwards.
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([span_json(
            "trace-still-alive",
            "s1",
            "svc",
            "op",
            1
        )])),
    );
    assert_eq!(status, 200);
    let (status, _) = server.request("GET", "/v1/traces/trace-still-alive", None);
    assert_eq!(status, 200);
    server.kill();
}

#[test]
fn server_missing_trace_preserves_error_contract() {
    let dir = test_dir("missing");
    let server = Server::spawn(&dir);

    let (status, body) = server.request("GET", "/v1/traces/never-written", None);
    assert_eq!(status, 404);
    assert_eq!(body["error"], "trace not found");

    let (status, body) = server.request("GET", "/v1/nonsense", None);
    assert_eq!(status, 404);
    assert_eq!(body["error"], "not found");
    server.kill();
}

#[test]
fn server_accepts_the_documented_wire_contract() {
    // The README quickstart verbatim: OTel-style timestamp names, no events,
    // no parent — plus an undocumented extra field that must survive the
    // round trip ("any other fields you send are stored and returned
    // verbatim"). Review finding: typed deserialization had silently
    // narrowed all of this.
    let dir = test_dir("wire");
    let server = Server::spawn(&dir);

    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "trace-1",
            "span_id": "span-1",
            "name": "charge",
            "service": "checkout",
            "start_time_unix_nano": 1_700_000_000_000_000_000u64,
            "end_time_unix_nano": 1_700_000_000_002_500_000u64,
            "status": "ok",
            "attributes": {"region": "us-east", "http.method": "POST"},
            "vendor.custom": "kept-verbatim"
        }])),
    );
    assert_eq!(status, 200, "quickstart-shaped ingest must succeed: {body}");
    assert_eq!(body["accepted"], 1);

    // The bench aliases too.
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "trace-1",
            "span_id": "span-2",
            "name": "operation",
            "service": "checkout",
            "start_ns": 1_700_000_000_003_000_000u64,
            "end_ns": 1_700_000_000_003_500_000u64
        }])),
    );
    assert_eq!(status, 200, "bench-alias ingest must succeed: {body}");

    let (status, body) = server.request("GET", "/v1/traces/trace-1", None);
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0]["start_time_ns"], 1_700_000_000_000_000_000u64);
    assert_eq!(
        spans[0]["vendor.custom"], "kept-verbatim",
        "unknown fields must survive the round trip: {body}"
    );
    server.kill();
}

#[test]
fn server_supports_the_documented_filters() {
    let dir = test_dir("filters");
    let server = Server::spawn(&dir);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {
                "trace_id": "t1", "span_id": "fast", "name": "op", "service": "svc",
                "start_time_ns": 1_000_000u64, "end_time_ns": 2_000_000u64,
                "attributes": {"region": "us-east", "retries": 3}
            },
            {
                "trace_id": "t1", "span_id": "slow", "name": "op", "service": "svc",
                "start_time_ns": 5_000_000u64, "end_time_ns": 55_000_000u64,
                "attributes": {"region": "eu-west"}
            }
        ])),
    );
    assert_eq!(status, 200);

    // attr.KEY: bare string value.
    let (status, body) = server.request("GET", "/v1/spans?attr.region=us-east", None);
    assert_eq!(status, 200, "attr filter must be accepted: {body}");
    let spans = body.as_array().expect("array");
    assert_eq!(spans.len(), 1, "{body}");
    assert_eq!(spans[0]["span_id"], "fast");

    // attr.KEY: JSON literal matches a typed value.
    let (status, body) = server.request("GET", "/v1/spans?attr.retries=3", None);
    assert_eq!(status, 200);
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    // min_duration_ms in milliseconds.
    let (status, body) = server.request("GET", "/v1/spans?min_duration_ms=10", None);
    assert_eq!(status, 200, "min_duration_ms must be accepted: {body}");
    let spans = body.as_array().expect("array");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["span_id"], "slow");

    // since/until in Unix nanoseconds.
    let (status, body) = server.request("GET", "/v1/spans?since=4000000&until=6000000", None);
    assert_eq!(status, 200, "since/until must be accepted: {body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    // Documented stats keys.
    let (status, body) = server.request("GET", "/v1/stats", None);
    assert_eq!(status, 200);
    for key in ["span_count", "segment_count", "bytes_on_disk"] {
        assert!(body.get(key).is_some(), "stats must expose {key}: {body}");
    }
    server.kill();
}
