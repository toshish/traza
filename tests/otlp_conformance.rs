//! OTLP/HTTP JSON conformance: the leg-2 fixture drives the real server
//! binary end to end and reads mapped spans back through the existing API.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_traza-server"))
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines
                .next()
                .expect("server exited before announcing its port")
                .expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", self.port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 50 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect failed: {error}"),
                }
            }
        };
        let body_len = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body writes");
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
            .map(|body| serde_json::from_str(body).expect("json body"))
            .unwrap_or(Value::Null);
        (status, payload)
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("traza-otlp-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// The leg-2 spec fixture: two resources, two scopes, typed attributes,
/// string-encoded nanos, events, and a STATUS_CODE_ERROR span.
fn fixture() -> Value {
    json!({
      "resourceSpans": [
        {
          "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "checkout"}},
            {"key": "deployment", "value": {"stringValue": "prod"}}
          ]},
          "scopeSpans": [
            {
              "scope": {"name": "lib-a", "attributes": [
                {"key": "scope.tag", "value": {"stringValue": "alpha"}}
              ]},
              "spans": [
                {
                  "traceId": "0AF7651916CD43DD8448EB211C80319C",
                  "spanId": "B7AD6B7169203331",
                  "name": "charge",
                  "startTimeUnixNano": "1700000001000000000",
                  "endTimeUnixNano": "1700000001002500000",
                  "status": {"code": "STATUS_CODE_OK"},
                  "attributes": [
                    {"key": "retries", "value": {"intValue": "3"}},
                    {"key": "amount", "value": {"doubleValue": 12.5}},
                    {"key": "scope.tag", "value": {"stringValue": "span-wins"}}
                  ],
                  "events": [
                    {"name": "authorized", "timeUnixNano": "1700000001001000000",
                     "attributes": [{"key": "gateway", "value": {"stringValue": "g1"}}]}
                  ]
                }
              ]
            },
            {
              "scope": {"name": "lib-b"},
              "spans": [
                {
                  "traceId": "0af7651916cd43dd8448eb211c80319c",
                  "spanId": "00f067aa0ba902b7",
                  "parentSpanId": "b7ad6b7169203331",
                  "name": "emit-receipt",
                  "startTimeUnixNano": 1700000001003000000u64,
                  "endTimeUnixNano": 1700000001004000000u64,
                  "status": {"code": 2}
                }
              ]
            }
          ]
        },
        {
          "resource": {},
          "scopeSpans": [
            {"spans": [
              {
                "traceId": "ffffffffffffffffffffffffffffffff",
                "spanId": "1111111111111111",
                "name": "orphan",
                "startTimeUnixNano": "1700000002000000000",
                "endTimeUnixNano": "1700000002000100000"
              }
            ]}
          ]
        }
      ]
    })
}

#[test]
fn otlp_request_maps_onto_the_span_model() {
    let dir = test_dir("map");
    let server = Server::spawn(&dir);

    let (status, body) = server.request("POST", "/v1/traces", Some(&fixture()));
    assert_eq!(status, 200, "OTLP ingest failed: {body}");
    assert!(
        body.get("partialSuccess").is_some(),
        "OTLP success shape: {body}"
    );

    let (status, body) = server.request("GET", "/v1/traces/0af7651916cd43dd8448eb211c80319c", None);
    assert_eq!(status, 200, "trace read failed: {body}");
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 2, "both scope spans mapped: {body}");
    let charge = &spans[0];
    assert_eq!(charge["span_id"], "b7ad6b7169203331", "hex id lowercased");
    assert_eq!(charge["service"], "checkout", "service.name from resource");
    assert_eq!(charge["status"], "ok");
    assert_eq!(
        charge["start_time_ns"], 1_700_000_001_000_000_000u64,
        "string nanos"
    );
    assert_eq!(
        charge["attributes"]["retries"], 3,
        "intValue string -> number"
    );
    assert_eq!(charge["attributes"]["amount"], 12.5);
    assert_eq!(
        charge["attributes"]["scope.tag"], "span-wins",
        "span attributes win over scope attributes"
    );
    assert_eq!(charge["events"][0]["name"], "authorized");
    let receipt = &spans[1];
    assert_eq!(receipt["parent_span_id"], "b7ad6b7169203331");
    assert_eq!(receipt["status"], "error", "numeric status code 2");

    let (status, body) = server.request("GET", "/v1/traces/ffffffffffffffffffffffffffffffff", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"][0]["service"], "unknown_service",
        "missing service.name falls back"
    );

    // OTLP spans are queryable through the existing filter API (indexes).
    let (status, body) = server.request("GET", "/v1/spans?service=checkout", None);
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(2),
        "service filter: {body}"
    );
    let (status, body) = server.request("GET", "/v1/spans?attr.retries=3", None);
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(1),
        "attr filter: {body}"
    );
    server.kill();
}

#[test]
fn malformed_otlp_is_rejected() {
    let dir = test_dir("bad");
    let server = Server::spawn(&dir);
    let (status, body) = server.request("POST", "/v1/traces", Some(&json!({"nope": []})));
    assert_eq!(status, 400, "structurally invalid must 400: {body}");
    let (status, _) = server.request(
        "POST",
        "/v1/traces",
        Some(&json!({"resourceSpans": [{"scopeSpans": [{"spans": [{"traceId": "zz", "spanId": "11", "name": "x", "startTimeUnixNano": "1", "endTimeUnixNano": "2"}]}]}]})),
    );
    assert_eq!(status, 400, "non-hex trace id must 400");
    // The server survives rejects.
    let (status, _) = server.request("GET", "/v1/stats", None);
    assert_eq!(status, 200);
    server.kill();
}
