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
        let response = read_until_close(&mut stream);
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

/// Reads until the server closes the socket, tolerating a close delivered as
/// RST once the response is complete — a loaded kernel turns the server's
/// post-response close into a reset rather than a FIN-drain, and `read_to_end`
/// then errors AFTER handing over every byte (the lesson of `tests/auth.rs`).
/// An INCOMPLETE response still panics.
fn read_until_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        assert!(
            complete_http_response(&response),
            "incomplete response after {:?}: {error}",
            error.kind()
        );
    }
    response
}

/// True once `response` holds a full header block plus the `Content-Length`
/// bytes it declares.
fn complete_http_response(response: &[u8]) -> bool {
    let Some(header_end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&response[..header_end]) else {
        return false;
    };
    let Some(content_length) = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    }) else {
        return false;
    };
    response.len() >= header_end + 4 + content_length
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

    // Span A uses the CURRENT OTel GenAI names captured from OpenLLMetry:
    // gen_ai.provider.name, gen_ai.operation.name, input/output tokens, and
    // JSON gen_ai.input.messages / gen_ai.output.messages (role + parts).
    let input_messages =
        r#"[{"role":"user","parts":[{"type":"text","content":"What is Traza?"}]}]"#;
    let output_messages = r#"[{"role":"assistant","parts":[{"type":"text","content":"A trace datastore."}],"finish_reason":"stop"}]"#;
    // Span B uses the OTel-DEPRECATED names still emitted by older
    // instrumentation: gen_ai.system + prompt/completion tokens + indexed
    // gen_ai.prompt.N / gen_ai.completion.N messages.
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {
                "trace_id": "ta", "span_id": "a1", "name": "openai.chat", "service": "agent",
                "start_time_ns": 1_000_000_000u64, "end_time_ns": 1_002_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.provider.name": "openai",
                    "gen_ai.operation.name": "chat",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.usage.input_tokens": 120,
                    "gen_ai.usage.output_tokens": 80,
                    "llm.usage.total_tokens": 200,
                    "gen_ai.conversation.id": "chat-1",
                    "traceloop.span.kind": "llm",
                    "gen_ai.input.messages": input_messages,
                    "gen_ai.output.messages": output_messages
                }
            },
            {
                "trace_id": "tb", "span_id": "b1", "name": "anthropic.chat", "service": "agent",
                "start_time_ns": 2_000_000_000u64, "end_time_ns": 2_003_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.system": "anthropic",
                    "gen_ai.request.model": "claude-sonnet",
                    "gen_ai.usage.prompt_tokens": 10,
                    "gen_ai.usage.completion_tokens": 5,
                    "gen_ai.conversation.id": "chat-1",
                    "gen_ai.prompt.0.role": "user",
                    "gen_ai.prompt.0.content": "Hi",
                    "gen_ai.completion.0.role": "assistant",
                    "gen_ai.completion.0.content": "Hello"
                }
            },
            {
                "trace_id": "tc", "span_id": "c1", "name": "openai.chat", "service": "worker",
                "start_time_ns": 3_000_000_000u64, "end_time_ns": 3_001_000_000u64, "status": "ok",
                "attributes": {
                    "gen_ai.provider.name": "openai",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.usage.input_tokens": 30,
                    "gen_ai.usage.output_tokens": 20,
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
                    {"key": "gen_ai.provider.name", "value": {"stringValue": "openai"}},
                    {"key": "gen_ai.request.model", "value": {"stringValue": "gpt-4o"}},
                    {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "40"}},
                    {"key": "gen_ai.usage.output_tokens", "value": {"intValue": "10"}},
                    {"key": "gen_ai.conversation.id", "value": {"stringValue": "chat-1"}}
                ]
            }]}]
        }]})),
    );
    assert_eq!(status, 200, "OTLP ingest: {body}");

    // Grouped by provider — a dimension that did not exist. openai comes from
    // gen_ai.provider.name (A, C, D); anthropic from the deprecated
    // gen_ai.system alias (B).
    let (status, body) = server.request("GET", "/v1/stats/llm?group_by=provider", None);
    assert_eq!(status, 200);
    let openai = row_by(&body["rows"], "key", "openai");
    assert_eq!(openai["llm_calls"], 3, "openai spans A, C, D: {body}");
    assert_eq!(openai["total_tokens"], 300, "200 + 50 + 50");
    let anthropic = row_by(&body["rows"], "key", "anthropic");
    assert_eq!(anthropic["llm_calls"], 1);
    assert_eq!(
        anthropic["total_tokens"], 15,
        "input+output fallback via deprecated names: 10 + 5"
    );

    // Grouped by model — resolved from gen_ai.request.model.
    let (status, body) = server.request("GET", "/v1/stats/llm?group_by=model", None);
    assert_eq!(status, 200);
    let gpt = row_by(&body["rows"], "key", "gpt-4o");
    assert_eq!(gpt["llm_calls"], 3);
    assert_eq!(gpt["total_tokens"], 300);

    // Span A round-trips the current-shape JSON messages verbatim (the UI
    // parses them; the store keeps them intact).
    let (status, body) = server.request("GET", "/v1/traces/ta", None);
    assert_eq!(status, 200);
    let a_attrs = &body["spans"][0]["attributes"];
    assert_eq!(a_attrs["gen_ai.input.messages"], json!(input_messages));
    assert_eq!(a_attrs["gen_ai.output.messages"], json!(output_messages));

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
        body["spans"].as_array().map(Vec::len),
        Some(1),
        "only span A: {body}"
    );
    assert_eq!(body["spans"][0]["span_id"], "a1");

    server.kill();
}

#[test]
fn session_filter_unions_mixed_convention_keys() {
    // A single session whose spans use DIFFERENT session keys — one native
    // `session.id`, one OpenLLMetry `gen_ai.conversation.id`. This is the
    // migration case the reviewer flagged: a single-key attr filter drops
    // half the session; the dedicated `session=` filter must return it whole.
    let dir = test_dir("mixed");
    let server = Server::spawn(&dir);
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {
                "trace_id": "m1", "span_id": "s1", "name": "openai.chat", "service": "agent",
                "start_time_ns": 1_000_000_000u64, "end_time_ns": 1_001_000_000u64, "status": "ok",
                "attributes": {"session.id": "mix", "gen_ai.request.model": "gpt-4o",
                               "gen_ai.usage.input_tokens": 5, "gen_ai.usage.output_tokens": 5}
            },
            {
                "trace_id": "m2", "span_id": "s2", "name": "openai.chat", "service": "agent",
                "start_time_ns": 2_000_000_000u64, "end_time_ns": 2_001_000_000u64, "status": "ok",
                "attributes": {"gen_ai.conversation.id": "mix", "gen_ai.request.model": "gpt-4o",
                               "gen_ai.usage.input_tokens": 7, "gen_ai.usage.output_tokens": 3}
            }
        ])),
    );
    assert_eq!(status, 200);

    // The session rollup sees both spans...
    let (status, body) = server.request("GET", "/v1/sessions/mix", None);
    assert_eq!(status, 200);
    assert_eq!(body["span_count"], 2, "both conventions join one session");
    assert_eq!(body["total_tokens"], 20);

    // ...and so does the dedicated session filter (the union).
    let (status, body) = server.request("GET", "/v1/spans?session=mix", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(2),
        "session filter unions both keys: {body}"
    );

    // A single-key attr filter, by contrast, only sees its own dialect — the
    // exact drop the dedicated filter fixes.
    let (status, body) = server.request("GET", "/v1/spans?attr.session.id=mix", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(1),
        "single key sees half"
    );

    // The session filter composes with the other predicates (AND).
    let (status, body) = server.request("GET", "/v1/spans?session=mix&name=nope", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(0),
        "name predicate still applies"
    );

    server.kill();
}

#[test]
fn a_numeric_session_id_can_be_listed_and_opened() {
    // Normalization stringifies numeric attributes, so a numeric
    // gen_ai.conversation.id LISTS as a session. It must also be openable:
    // matching only the JSON string left such a session visible but dead.
    let dir = test_dir("numeric-session");
    let server = Server::spawn(&dir);

    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "tn", "span_id": "n1", "name": "openai.chat", "service": "agent",
            "start_time_ns": 1_000_000_000u64, "end_time_ns": 1_002_000_000u64, "status": "ok",
            "attributes": {
                "gen_ai.provider.name": "openai",
                "gen_ai.request.model": "gpt-4o",
                "gen_ai.usage.input_tokens": 5,
                "gen_ai.usage.output_tokens": 5,
                "gen_ai.conversation.id": 4711
            }
        }])),
    );
    assert_eq!(status, 200, "ingest: {body}");

    let (status, body) = server.request("GET", "/v1/sessions", None);
    assert_eq!(status, 200);
    let session = row_by(&body["sessions"], "session_id", "4711");
    assert_eq!(session["span_count"], 1, "numeric id lists: {body}");

    let (status, body) = server.request("GET", "/v1/sessions/4711", None);
    assert_eq!(status, 200, "and it opens: {body}");
    assert_eq!(body["span_count"], 1);

    let (status, body) = server.request("GET", "/v1/spans?session=4711", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(1),
        "and its spans are reachable: {body}"
    );

    server.kill();
}

#[test]
fn a_re_ingested_span_is_counted_once_in_its_session() {
    // The union across session keys must resolve under ONE snapshot: a span
    // re-ingested under a different recognized key used to be seen first in
    // its superseded version, which then locked the newer version out.
    let dir = test_dir("session-supersede");
    let server = Server::spawn(&dir);

    let post = |attributes: serde_json::Value| {
        json!([{
            "trace_id": "ts", "span_id": "s1", "name": "openai.chat", "service": "agent",
            "start_time_ns": 1_000_000_000u64, "end_time_ns": 1_002_000_000u64, "status": "ok",
            "attributes": attributes
        }])
    };

    // First version: grouped by the native key.
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&post(json!({
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.usage.input_tokens": 10,
            "gen_ai.usage.output_tokens": 10,
            "session.id": "mix"
        }))),
    );
    assert_eq!(status, 200);
    server.request("POST", "/v1/flush", None);

    // Re-ingested under the SAME primary key, now carrying both keys and
    // different usage. Last write wins: 40 tokens, counted once.
    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&post(json!({
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.usage.input_tokens": 20,
            "gen_ai.usage.output_tokens": 20,
            "session.id": "mix",
            "gen_ai.conversation.id": "mix"
        }))),
    );
    assert_eq!(status, 200);

    let (status, body) = server.request("GET", "/v1/sessions/mix", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["span_count"], 1, "one primary key, one span: {body}");
    assert_eq!(body["total_tokens"], 40, "the NEWEST version wins: {body}");

    let (status, body) = server.request("GET", "/v1/spans?session=mix", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(1),
        "no duplicate from the key union: {body}"
    );

    server.kill();
}
