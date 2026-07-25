//! OTLP/HTTP binary-protobuf conformance against the real server binary.
//!
//! The test carries its own tiny protobuf ENCODER (an independent
//! implementation of the wire format), so agreement between it and the
//! server's decoder is evidence about the format, not a shared bug.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// ------------------------------------------------------------ test encoder

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn tag(field: u64, wire_type: u8, out: &mut Vec<u8>) {
    varint(field << 3 | u64::from(wire_type), out);
}

fn bytes_field(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
    tag(field, 2, out);
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn string_field(field: u64, text: &str, out: &mut Vec<u8>) {
    bytes_field(field, text.as_bytes(), out);
}

fn fixed64_field(field: u64, value: u64, out: &mut Vec<u8>) {
    tag(field, 1, out);
    out.extend_from_slice(&value.to_le_bytes());
}

fn varint_field(field: u64, value: u64, out: &mut Vec<u8>) {
    tag(field, 0, out);
    varint(value, out);
}

fn any_string(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    string_field(1, text, &mut out);
    out
}

fn any_int(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    varint_field(3, value as u64, &mut out);
    out
}

fn any_double(value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    tag(4, 1, &mut out);
    out.extend_from_slice(&value.to_bits().to_le_bytes());
    out
}

fn any_bool(value: bool) -> Vec<u8> {
    let mut out = Vec::new();
    varint_field(2, u64::from(value), &mut out);
    out
}

fn any_array(values: &[Vec<u8>]) -> Vec<u8> {
    let mut list = Vec::new();
    for value in values {
        bytes_field(1, value, &mut list);
    }
    let mut out = Vec::new();
    bytes_field(5, &list, &mut out);
    out
}

fn any_kvlist(pairs: &[Vec<u8>]) -> Vec<u8> {
    let mut list = Vec::new();
    for pair in pairs {
        bytes_field(1, pair, &mut list);
    }
    let mut out = Vec::new();
    bytes_field(6, &list, &mut out);
    out
}

fn any_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    bytes_field(7, value, &mut out);
    out
}

fn key_value(key: &str, any_value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    string_field(1, key, &mut out);
    bytes_field(2, any_value, &mut out);
    out
}

// ---------------------------------------------------------------- harness

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
            .env_remove("TRAZA_TOKENS")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines.next().expect("port line").expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
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

    fn post_protobuf(&self, body: &[u8]) -> (u16, String) {
        self.post(body, "application/x-protobuf")
    }

    fn post_otlp_json(&self, body: &Value) -> (u16, String) {
        self.post(
            &serde_json::to_vec(body).expect("encodes"),
            "application/json",
        )
    }

    fn post(&self, body: &[u8], content_type: &str) -> (u16, String) {
        let mut stream = self.connect();
        write!(
            stream,
            "POST /v1/traces HTTP/1.1\r\nHost: x\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("writes");
        stream.write_all(body).expect("body");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads");
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        (status, text)
    }

    fn get_json(&self, target: &str) -> (u16, Value) {
        let mut stream = self.connect();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
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
}

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
        "traza-otlppb-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// The full request: resource(service.name=checkout) -> scope -> two spans,
/// the second linking to the first across traces.
fn sample_request() -> Vec<u8> {
    let trace_a = [0xAA_u8; 16];
    let trace_b = [0xBB_u8; 16];
    let span_1 = [0x01_u8; 8];
    let span_2 = [0x02_u8; 8];

    // Span 1: attributes of every scalar shape + an event + error status.
    let mut span1 = Vec::new();
    bytes_field(1, &trace_a, &mut span1);
    bytes_field(2, &span_1, &mut span1);
    string_field(5, "charge-card", &mut span1);
    fixed64_field(7, 1_000_000, &mut span1);
    fixed64_field(8, 2_000_000, &mut span1);
    bytes_field(9, &key_value("llm.model", &any_string("m-pb")), &mut span1);
    bytes_field(9, &key_value("llm.prompt_tokens", &any_int(42)), &mut span1);
    bytes_field(
        9,
        &key_value("llm.cost_usd", &any_double(0.125)),
        &mut span1,
    );
    let mut event = Vec::new();
    fixed64_field(1, 1_500_000, &mut event);
    string_field(2, "retry", &mut event);
    bytes_field(3, &key_value("attempt", &any_int(2)), &mut event);
    bytes_field(11, &event, &mut span1);
    let mut status = Vec::new();
    string_field(2, "boom", &mut status);
    varint_field(3, 2, &mut status); // STATUS_CODE_ERROR
    bytes_field(15, &status, &mut span1);

    // Span 2 (different trace): links back to span 1.
    let mut link = Vec::new();
    bytes_field(1, &trace_a, &mut link);
    bytes_field(2, &span_1, &mut link);
    bytes_field(
        4,
        &key_value("relation", &any_string("retry-of")),
        &mut link,
    );
    let mut span2 = Vec::new();
    bytes_field(1, &trace_b, &mut span2);
    bytes_field(2, &span_2, &mut span2);
    string_field(5, "emit-receipt", &mut span2);
    fixed64_field(7, 3_000_000, &mut span2);
    fixed64_field(8, 4_000_000, &mut span2);
    bytes_field(13, &link, &mut span2);

    let mut scope_spans = Vec::new();
    let mut scope = Vec::new();
    string_field(1, "traza-test", &mut scope);
    bytes_field(1, &scope, &mut scope_spans);
    bytes_field(2, &span1, &mut scope_spans);
    bytes_field(2, &span2, &mut scope_spans);

    let mut resource = Vec::new();
    bytes_field(
        1,
        &key_value("service.name", &any_string("checkout")),
        &mut resource,
    );

    let mut resource_spans = Vec::new();
    bytes_field(1, &resource, &mut resource_spans);
    bytes_field(2, &scope_spans, &mut resource_spans);

    let mut request = Vec::new();
    bytes_field(1, &resource_spans, &mut request);
    request
}

#[test]
fn protobuf_export_round_trips_through_the_engine() {
    let dir = test_dir("roundtrip");
    let server = Server::spawn(&dir);

    let (status, raw) = server.post_protobuf(&sample_request());
    assert_eq!(status, 200, "{raw}");
    assert!(
        raw.to_ascii_lowercase()
            .contains("content-type: application/x-protobuf"),
        "protobuf clients get a protobuf-typed response: {raw}"
    );

    // Span 1: ids hex-lowercased, attrs flattened, event mapped, status error.
    let trace_a = "aa".repeat(16);
    let (status, body) = server.get_json(&format!("/v1/traces/{trace_a}"));
    assert_eq!(status, 200, "{body}");
    let span = &body["spans"][0];
    assert_eq!(span["span_id"], "01".repeat(8));
    assert_eq!(span["name"], "charge-card");
    assert_eq!(span["service"], "checkout");
    assert_eq!(span["status"], "error");
    assert_eq!(span["attributes"]["llm.model"], "m-pb");
    assert_eq!(span["attributes"]["llm.prompt_tokens"], 42);
    assert_eq!(span["attributes"]["llm.cost_usd"], 0.125);
    assert_eq!(span["events"][0]["name"], "retry");
    assert_eq!(span["events"][0]["attributes"]["attempt"], 2);

    // Span 2: the cross-trace link survives with its attributes.
    let trace_b = "bb".repeat(16);
    let (status, body) = server.get_json(&format!("/v1/traces/{trace_b}"));
    assert_eq!(status, 200, "{body}");
    let span = &body["spans"][0];
    assert_eq!(span["links"][0]["trace_id"], trace_a);
    assert_eq!(span["links"][0]["span_id"], "01".repeat(8));
    assert_eq!(span["links"][0]["attributes"]["relation"], "retry-of");
}

#[test]
fn malformed_protobuf_is_rejected_not_crashed() {
    let dir = test_dir("malformed");
    let server = Server::spawn(&dir);

    // Truncated varint: a lone continuation byte.
    let (status, _) = server.post_protobuf(&[0x80]);
    assert_eq!(status, 400);
    // Length running past the buffer.
    let mut bad = Vec::new();
    tag(1, 2, &mut bad);
    varint(1_000_000, &mut bad);
    bad.push(0x00);
    let (status, _) = server.post_protobuf(&bad);
    assert_eq!(status, 400);
    // Hostile nesting: kvlist 40 levels deep must hit the depth cap.
    let mut value = any_string("leaf");
    for _ in 0..40 {
        let mut kvlist_value = Vec::new();
        bytes_field(1, &key_value("k", &value), &mut kvlist_value);
        let mut wrapped = Vec::new();
        bytes_field(6, &kvlist_value, &mut wrapped);
        value = wrapped;
    }
    let mut span = Vec::new();
    bytes_field(1, &[0xAA; 16], &mut span);
    bytes_field(2, &[0x01; 8], &mut span);
    string_field(5, "deep", &mut span);
    fixed64_field(7, 1, &mut span);
    fixed64_field(8, 2, &mut span);
    bytes_field(9, &key_value("nested", &value), &mut span);
    let mut scope_spans = Vec::new();
    bytes_field(2, &span, &mut scope_spans);
    let mut resource_spans = Vec::new();
    bytes_field(2, &scope_spans, &mut resource_spans);
    let mut request = Vec::new();
    bytes_field(1, &resource_spans, &mut request);
    let (status, _) = server.post_protobuf(&request);
    assert_eq!(status, 400, "hostile nesting must be rejected");

    // Unknown fields are skipped, not fatal: an empty request plus a stray
    // field the schema does not define.
    let mut benign = Vec::new();
    varint_field(99, 7, &mut benign);
    let (status, _) = server.post_protobuf(&benign);
    assert_eq!(status, 200, "unknown fields skip cleanly");

    // Healthy afterwards.
    let (status, _) = server.get_json("/v1/stats");
    assert_eq!(status, 200);
}

#[test]
fn json_links_round_trip_on_the_native_path() {
    // Links are first-class on the native JSON surface too.
    let dir = test_dir("json-links");
    let server = Server::spawn(&dir);
    let mut stream = server.connect();
    let body = serde_json::to_vec(&serde_json::json!([{
        "trace_id": "t-json", "span_id": "s1", "name": "op", "service": "svc",
        "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "links": [{"trace_id": "t-other", "span_id": "s9",
                    "attributes": {"relation": "follows"}}]
    }]))
    .expect("encodes");
    write!(
        stream,
        "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("writes");
    stream.write_all(&body).expect("body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("reads");
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));

    let (status, body) = server.get_json("/v1/traces/t-json");
    assert_eq!(status, 200);
    assert_eq!(body["spans"][0]["links"][0]["trace_id"], "t-other");
    assert_eq!(
        body["spans"][0]["links"][0]["attributes"]["relation"],
        "follows"
    );
}

// -------------------------------------------- protobuf vs JSON, same payload

/// One logical request, hand-encoded as protobuf.
///
/// Paired with [`differential_json`], which encodes the SAME request as
/// OTLP/HTTP JSON. Every `AnyValue` variant appears, including the nested ones
/// and the one the two encodings spell differently on the wire (bytes: raw
/// here, base64 there), because the two encodings are handled by two
/// independent decoders that must agree.
fn differential_protobuf() -> Vec<u8> {
    let mut span1 = Vec::new();
    bytes_field(1, &[0xAA_u8; 16], &mut span1);
    bytes_field(2, &[0x01_u8; 8], &mut span1);
    bytes_field(4, &[0x09_u8; 8], &mut span1);
    string_field(5, "charge", &mut span1);
    fixed64_field(7, 1_000_000, &mut span1);
    fixed64_field(8, 2_000_000, &mut span1);
    bytes_field(9, &key_value("a.string", &any_string("s")), &mut span1);
    bytes_field(9, &key_value("a.int", &any_int(42)), &mut span1);
    bytes_field(9, &key_value("a.double", &any_double(0.5)), &mut span1);
    bytes_field(9, &key_value("a.bool", &any_bool(true)), &mut span1);
    bytes_field(
        9,
        &key_value(
            "a.array",
            &any_array(&[any_string("x"), any_int(7), any_bool(false)]),
        ),
        &mut span1,
    );
    bytes_field(
        9,
        &key_value(
            "a.kvlist",
            &any_kvlist(&[
                key_value("inner", &any_string("deep")),
                key_value("n", &any_int(3)),
            ]),
        ),
        &mut span1,
    );
    bytes_field(9, &key_value("a.bytes", &any_bytes(&[1, 2, 3])), &mut span1);
    bytes_field(
        9,
        &key_value("shared", &any_string("from-span")),
        &mut span1,
    );
    let mut event = Vec::new();
    fixed64_field(1, 1_500_000, &mut event);
    string_field(2, "retry", &mut event);
    bytes_field(3, &key_value("attempt", &any_int(2)), &mut event);
    bytes_field(11, &event, &mut span1);
    let mut link = Vec::new();
    bytes_field(1, &[0xBB_u8; 16], &mut link);
    bytes_field(2, &[0x02_u8; 8], &mut link);
    bytes_field(
        4,
        &key_value("relation", &any_string("retry-of")),
        &mut link,
    );
    bytes_field(13, &link, &mut span1);
    let mut status = Vec::new();
    string_field(2, "boom", &mut status);
    varint_field(3, 2, &mut status);
    bytes_field(15, &status, &mut span1);

    // No name, no status, no attributes: the defaults have to match too.
    let mut span2 = Vec::new();
    bytes_field(1, &[0xAA_u8; 16], &mut span2);
    bytes_field(2, &[0x03_u8; 8], &mut span2);
    fixed64_field(7, 3_000_000, &mut span2);
    fixed64_field(8, 4_000_000, &mut span2);

    let mut scope = Vec::new();
    string_field(1, "lib", &mut scope);
    bytes_field(3, &key_value("scope.tag", &any_string("alpha")), &mut scope);
    bytes_field(
        3,
        &key_value("shared", &any_string("from-scope")),
        &mut scope,
    );

    let mut scope_spans = Vec::new();
    bytes_field(1, &scope, &mut scope_spans);
    bytes_field(2, &span1, &mut scope_spans);
    bytes_field(2, &span2, &mut scope_spans);

    let mut resource = Vec::new();
    bytes_field(
        1,
        &key_value("service.name", &any_string("checkout")),
        &mut resource,
    );
    bytes_field(
        1,
        &key_value("deployment", &any_string("prod")),
        &mut resource,
    );

    let mut resource_spans = Vec::new();
    bytes_field(1, &resource, &mut resource_spans);
    bytes_field(2, &scope_spans, &mut resource_spans);

    let mut request = Vec::new();
    bytes_field(1, &resource_spans, &mut request);
    request
}

/// [`differential_protobuf`] in canonical proto3 JSON: 64-bit ints as strings,
/// status codes as enum names, bytes as base64.
fn differential_json() -> Value {
    serde_json::json!({
      "resourceSpans": [{
        "resource": {"attributes": [
          {"key": "service.name", "value": {"stringValue": "checkout"}},
          {"key": "deployment", "value": {"stringValue": "prod"}}
        ]},
        "scopeSpans": [{
          "scope": {"name": "lib", "attributes": [
            {"key": "scope.tag", "value": {"stringValue": "alpha"}},
            {"key": "shared", "value": {"stringValue": "from-scope"}}
          ]},
          "spans": [
            {
              "traceId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "spanId": "0101010101010101",
              "parentSpanId": "0909090909090909",
              "name": "charge",
              "startTimeUnixNano": "1000000",
              "endTimeUnixNano": "2000000",
              "status": {"message": "boom", "code": "STATUS_CODE_ERROR"},
              "attributes": [
                {"key": "a.string", "value": {"stringValue": "s"}},
                {"key": "a.int", "value": {"intValue": "42"}},
                {"key": "a.double", "value": {"doubleValue": 0.5}},
                {"key": "a.bool", "value": {"boolValue": true}},
                {"key": "a.array", "value": {"arrayValue": {"values": [
                  {"stringValue": "x"}, {"intValue": "7"}, {"boolValue": false}
                ]}}},
                {"key": "a.kvlist", "value": {"kvlistValue": {"values": [
                  {"key": "inner", "value": {"stringValue": "deep"}},
                  {"key": "n", "value": {"intValue": "3"}}
                ]}}},
                {"key": "a.bytes", "value": {"bytesValue": "AQID"}},
                {"key": "shared", "value": {"stringValue": "from-span"}}
              ],
              "events": [{
                "name": "retry", "timeUnixNano": "1500000",
                "attributes": [{"key": "attempt", "value": {"intValue": "2"}}]
              }],
              "links": [{
                "traceId": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "spanId": "0202020202020202",
                "attributes": [{"key": "relation", "value": {"stringValue": "retry-of"}}]
              }]
            },
            {
              "traceId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "spanId": "0303030303030303",
              "startTimeUnixNano": "3000000",
              "endTimeUnixNano": "4000000"
            }
          ]
        }]
      }]
    })
}

/// The two OTLP encodings are handled by two independent decoders. Nothing
/// makes them agree except the mapping rules they share, so the agreement is
/// asserted directly: same logical request in, identical spans out.
#[test]
fn otlp_json_and_protobuf_agree_on_one_payload() {
    let trace = "a".repeat(32);

    let pb_dir = test_dir("diff-pb");
    let pb_server = Server::spawn(&pb_dir);
    let (status, raw) = pb_server.post_protobuf(&differential_protobuf());
    assert_eq!(status, 200, "protobuf ingest: {raw}");
    let (status, from_protobuf) = pb_server.get_json(&format!("/v1/traces/{trace}"));
    assert_eq!(status, 200);
    drop(pb_server);

    let json_dir = test_dir("diff-json");
    let json_server = Server::spawn(&json_dir);
    let (status, raw) = json_server.post_otlp_json(&differential_json());
    assert_eq!(status, 200, "otlp json ingest: {raw}");
    let (status, from_json) = json_server.get_json(&format!("/v1/traces/{trace}"));
    assert_eq!(status, 200);
    drop(json_server);

    assert_eq!(
        from_protobuf, from_json,
        "the two OTLP encodings must map onto identical spans"
    );

    // Assert the payload exercised what it claims, so the comparison above
    // cannot pass by both sides being equally empty.
    let spans = from_json["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 2, "both spans landed");
    let first = spans
        .iter()
        .find(|span| span["span_id"] == "0101010101010101")
        .expect("span 1");
    assert_eq!(first["status"], "error");
    assert_eq!(first["parent_span_id"], "0909090909090909");
    assert_eq!(first["attributes"]["a.string"], "s");
    assert_eq!(first["attributes"]["a.int"], 42);
    assert_eq!(first["attributes"]["a.double"], 0.5);
    assert_eq!(first["attributes"]["a.bool"], true);
    assert_eq!(
        first["attributes"]["a.array"],
        serde_json::json!(["x", 7, false])
    );
    assert_eq!(
        first["attributes"]["a.kvlist"],
        serde_json::json!({"inner": "deep", "n": 3})
    );
    // bytesValue is stored as lowercase hex, like trace and span ids — the
    // protobuf raw bytes and the JSON base64 of the same three bytes have to
    // land on the one representation, or a consumer cannot read either.
    assert_eq!(first["attributes"]["a.bytes"], "010203");
    assert_eq!(
        first["attributes"]["scope.tag"], "alpha",
        "scope attributes merge in"
    );
    assert_eq!(
        first["attributes"]["shared"], "from-span",
        "span attributes win over scope"
    );
    assert_eq!(first["events"][0]["attributes"]["attempt"], 2);
    assert_eq!(first["links"][0]["attributes"]["relation"], "retry-of");
    let second = spans
        .iter()
        .find(|span| span["span_id"] == "0303030303030303")
        .expect("span 2");
    assert_eq!(second["name"], "", "an absent name defaults the same way");
    assert_eq!(second["status"], "");
    assert_eq!(second["service"], "checkout");
}
