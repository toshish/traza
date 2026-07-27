//! LLM-observability conventions, proven end to end: spans following
//! docs/llm-semantics.md are ingested through BOTH ingestion paths and every
//! documented query recipe returns the expected spans via the existing
//! filter API.

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
        std::env::temp_dir().join(format!("traza-llm-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn llm_span(
    trace: &str,
    span: &str,
    name: &str,
    service: &str,
    start: u64,
    duration_ms: u64,
    attrs: Value,
) -> Value {
    json!({
        "trace_id": trace, "span_id": span, "name": name, "service": service,
        "start_time_ns": start, "end_time_ns": start + duration_ms * 1_000_000,
        "status": "ok", "attributes": attrs,
        "events": [
            {"name": "llm.prompt", "timestamp_ns": start,
             "attributes": {"content": "What is Traza?"}},
            {"name": "llm.completion", "timestamp_ns": start + 1,
             "attributes": {"content": "A tracing datastore."}}
        ]
    })
}

#[test]
fn documented_recipes_are_served_by_the_filter_api() {
    let dir = test_dir("recipes");
    let server = Server::spawn(&dir);

    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            llm_span(
                "t1",
                "a1",
                "llm.completion",
                "agent-web",
                1_000_000_000,
                3000,
                json!({"llm.model": "gpt-5.6-sol", "llm.prompt_tokens": 120,
                       "llm.completion_tokens": 80, "llm.total_tokens": 200,
                       "llm.temperature": 0.7, "llm.stop_reason": "stop",
                       "llm.cost_usd": 0.012})
            ),
            llm_span(
                "t1",
                "a2",
                "llm.tool_call",
                "agent-web",
                2_000_000_000,
                400,
                json!({"llm.model": "gpt-5.6-sol", "llm.tool_name": "web_search"})
            ),
            llm_span(
                "t2",
                "b1",
                "llm.completion",
                "batch-worker",
                3_000_000_000,
                900,
                json!({"llm.model": "claude-fable-5", "llm.prompt_tokens": 50,
                       "llm.completion_tokens": 10, "llm.total_tokens": 60})
            ),
        ])),
    );
    assert_eq!(status, 200);

    // Recipe: all spans for one model (index-served attribute filter).
    let (status, body) = server.request("GET", "/v1/spans?attr.llm.model=gpt-5.6-sol", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(2),
        "model filter: {body}"
    );

    // Recipe: completions for one service.
    let (status, body) = server.request(
        "GET",
        "/v1/spans?service=agent-web&name=llm.completion",
        None,
    );
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("array");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["attributes"]["llm.total_tokens"], 200);

    // Recipe: slowest completions by duration.
    let (status, body) = server.request(
        "GET",
        "/v1/spans?name=llm.completion&min_duration_ms=2000",
        None,
    );
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("array");
    assert_eq!(spans.len(), 1, "only the 3s completion: {body}");
    assert_eq!(spans[0]["span_id"], "a1");

    // Recipe: tool-call frequency for one tool.
    let (status, body) = server.request(
        "GET",
        "/v1/spans?name=llm.tool_call&attr.llm.tool_name=web_search",
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(1));

    // Payload conventions: prompt/completion ride events verbatim.
    let (status, body) = server.request("GET", "/v1/traces/t1", None);
    assert_eq!(status, 200);
    let events = body["spans"][0]["events"].as_array().expect("events");
    assert_eq!(events[0]["name"], "llm.prompt");
    assert_eq!(events[0]["attributes"]["content"], "What is Traza?");
    server.kill();
}

#[test]
fn conventions_flow_through_otlp() {
    let dir = test_dir("otlp");
    let server = Server::spawn(&dir);
    let (status, body) = server.request(
        "POST",
        "/v1/traces",
        Some(&json!({"resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "agent-web"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "spanId": "cccccccccccccccc",
                "name": "llm.completion",
                "startTimeUnixNano": "1700000000000000000",
                "endTimeUnixNano": "1700000001500000000",
                "attributes": [
                    {"key": "llm.model", "value": {"stringValue": "gpt-5.6-sol"}},
                    {"key": "llm.total_tokens", "value": {"intValue": "200"}}
                ],
                "events": [{"name": "llm.completion",
                            "timeUnixNano": "1700000001000000000",
                            "attributes": [{"key": "content",
                                            "value": {"stringValue": "hi"}}]}]
            }]}]
        }]})),
    );
    assert_eq!(status, 200, "OTLP LLM span: {body}");

    let (status, body) = server.request("GET", "/v1/spans?attr.llm.model=gpt-5.6-sol", None);
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("array");
    assert_eq!(spans.len(), 1, "OTLP llm.model indexed: {body}");
    assert_eq!(spans[0]["attributes"]["llm.total_tokens"], 200);
    assert_eq!(spans[0]["events"][0]["attributes"]["content"], "hi");
    server.kill();
}
