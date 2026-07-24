//! OpenLLMetry / OpenTelemetry GenAI conformance, proven end to end: spans
//! following the Traceloop conventions (`gen_ai.*`, `llm.usage.*`,
//! `traceloop.*`) are ingested through BOTH surfaces (`/v1/spans` JSON and
//! OTLP `/v1/traces`) and land in Traza's derived views — sessions, provider
//! and model token/cost rollups — with no attribute renaming by the client.
//!
//! This is the regression guard for the gap the feature closed: an
//! OpenLLMetry-instrumented app used to store fine but register zero tokens,
//! cost, LLM calls, or sessions because Traza only knew its native `llm.*`
//! keys.

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
                    Err(error) => panic!("connect: {error}"),
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
            .map(|body| serde_json::from_str(body).expect("json"))
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
        std::env::temp_dir().join(format!("traza-ollm-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Finds the aggregate/session row whose `key`/`session_id` equals `id`.
fn row_by<'a>(rows: &'a Value, field: &str, id: &str) -> &'a Value {
    rows.as_array()
        .expect("array")
        .iter()
        .find(|row| row[field] == json!(id))
        .unwrap_or_else(|| panic!("no row with {field}={id} in {rows}"))
}

#[test]
fn openllmetry_spans_populate_sessions_and_rollups() {
    let dir = test_dir("recognize");
    let server = Server::spawn(&dir);

    // Three OpenLLMetry-shaped spans over the JSON surface. No native llm.*
    // keys, no session.id: everything is gen_ai.* / traceloop.*.
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {
                "trace_id": "ta", "span_id": "a1", "name": "openai.chat", "service": "agent",
                "start_time_ns": 1_000_000_000u64, "end_time_ns": 1_002_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.system": "openai",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.usage.prompt_tokens": 120,
                    "gen_ai.usage.completion_tokens": 80,
                    "llm.usage.total_tokens": 200,
                    "gen_ai.usage.cost": 0.01,
                    "gen_ai.conversation.id": "chat-1",
                    "traceloop.span.kind": "llm",
                    "gen_ai.prompt.0.role": "user",
                    "gen_ai.prompt.0.content": "Hi",
                    "gen_ai.completion.0.role": "assistant",
                    "gen_ai.completion.0.content": "Hello"
                }
            },
            {
                "trace_id": "tb", "span_id": "b1", "name": "anthropic.chat", "service": "agent",
                "start_time_ns": 2_000_000_000u64, "end_time_ns": 2_003_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.system": "anthropic",
                    "gen_ai.request.model": "claude-sonnet",
                    "gen_ai.usage.input_tokens": 10,
                    "gen_ai.usage.output_tokens": 5,
                    "gen_ai.conversation.id": "chat-1"
                }
            },
            {
                "trace_id": "tc", "span_id": "c1", "name": "openai.chat", "service": "worker",
                "start_time_ns": 3_000_000_000u64, "end_time_ns": 3_001_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.system": "openai",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.usage.prompt_tokens": 30,
                    "gen_ai.usage.completion_tokens": 20,
                    "traceloop.association.properties.chat_id": "chat-2"
                }
            }
        ])),
    );
    assert_eq!(status, 200, "JSON ingest: {body}");

    // A fourth span over OTLP/HTTP JSON — intValue token counters, same
    // conversation. Confirms gen_ai.* flows through the OTLP mapping too.
    let (status, body) = server.request(
        "POST",
        "/v1/traces",
        Some(&json!({"resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "agent"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": "dddddddddddddddddddddddddddddddd",
                "spanId": "dddddddddddddddd",
                "name": "openai.chat",
                "startTimeUnixNano": "4000000000",
                "endTimeUnixNano": "4001000000",
                "attributes": [
                    {"key": "gen_ai.system", "value": {"stringValue": "openai"}},
                    {"key": "gen_ai.request.model", "value": {"stringValue": "gpt-4o"}},
                    {"key": "gen_ai.usage.prompt_tokens", "value": {"intValue": "40"}},
                    {"key": "gen_ai.usage.completion_tokens", "value": {"intValue": "10"}},
                    {"key": "gen_ai.conversation.id", "value": {"stringValue": "chat-1"}}
                ]
            }]}]
        }]})),
    );
    assert_eq!(status, 200, "OTLP ingest: {body}");

    // Grouped by provider (gen_ai.system) — a dimension that did not exist.
    let (status, body) = server.request("GET", "/v1/stats/llm?group_by=provider", None);
    assert_eq!(status, 200);
    let openai = row_by(&body["rows"], "key", "openai");
    assert_eq!(openai["llm_calls"], 3, "openai spans A, C, D: {body}");
    assert_eq!(openai["total_tokens"], 300, "200 + 50 + 50");
    let anthropic = row_by(&body["rows"], "key", "anthropic");
    assert_eq!(anthropic["llm_calls"], 1);
    assert_eq!(
        anthropic["total_tokens"], 15,
        "input+output fallback: 10 + 5"
    );

    // Grouped by model — resolved from gen_ai.request.model.
    let (status, body) = server.request("GET", "/v1/stats/llm?group_by=model", None);
    assert_eq!(status, 200);
    let gpt = row_by(&body["rows"], "key", "gpt-4o");
    assert_eq!(gpt["llm_calls"], 3);
    assert_eq!(gpt["total_tokens"], 300);
    assert!(
        (gpt["cost_usd"].as_f64().expect("cost") - 0.01).abs() < 1e-9,
        "gen_ai.usage.cost is summed: {body}"
    );

    // Sessions grouped via gen_ai.conversation.id and a traceloop association
    // property — with the attribute that grouped each reported back.
    let (status, body) = server.request("GET", "/v1/sessions", None);
    assert_eq!(status, 200);
    let chat1 = row_by(&body["sessions"], "session_id", "chat-1");
    assert_eq!(chat1["span_count"], 3, "A, B, D: {body}");
    assert_eq!(chat1["trace_count"], 3, "ta, tb, and the OTLP trace");
    assert_eq!(chat1["total_tokens"], 265, "200 + 15 + 50");
    assert_eq!(chat1["session_attribute"], "gen_ai.conversation.id");
    let chat2 = row_by(&body["sessions"], "session_id", "chat-2");
    assert_eq!(chat2["span_count"], 1);
    assert_eq!(
        chat2["session_attribute"], "traceloop.association.properties.chat_id",
        "a session identified by an association property"
    );

    // Session detail resolves by the non-native key.
    let (status, body) = server.request("GET", "/v1/sessions/chat-1", None);
    assert_eq!(
        status, 200,
        "detail resolves via gen_ai.conversation.id: {body}"
    );
    assert_eq!(body["span_count"], 3);
    assert_eq!(body["session_attribute"], "gen_ai.conversation.id");
    assert_eq!(body["traces"].as_array().map(Vec::len), Some(3));

    // Portable "select the LLM spans" recipe: traceloop.span.kind is indexed
    // like any attribute (OpenLLMetry span NAMES are provider-specific).
    let (status, body) = server.request("GET", "/v1/spans?attr.traceloop.span.kind=llm", None);
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(1),
        "only span A: {body}"
    );
    assert_eq!(body[0]["span_id"], "a1");

    server.kill();
}
