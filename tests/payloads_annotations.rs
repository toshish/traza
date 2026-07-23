//! Payload offloading, annotations, and dataset export acceptance.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::annotations::Annotation;
use traza::{Config, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-payann-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span_with_content(trace_id: &str, span_id: &str, content: &str) -> traza::Span {
    serde_json::from_value(json!({
        "trace_id": trace_id, "span_id": span_id, "name": "llm.completion",
        "service": "agent", "start_time_ns": 1_000, "end_time_ns": 2_000,
        "attributes": {"llm.model": "m"},
        "events": [{"name": "llm.prompt", "timestamp_ns": 1_000,
                     "attributes": {"content": content}}],
    }))
    .expect("span")
}

#[test]
fn oversized_payloads_are_offloaded_and_readable() {
    let dir = test_dir("offload");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(1_024),
            ..Config::default()
        },
    )
    .expect("opens");

    let big = "P".repeat(10_000);
    store
        .ingest(span_with_content("t1", "s1", &big))
        .expect("ingests");
    // An identical payload from another span dedupes to the same file.
    store
        .ingest(span_with_content("t2", "s1", &big))
        .expect("ingests");
    // Small content stays inline.
    store
        .ingest(span_with_content("t3", "s1", "tiny"))
        .expect("ingests");

    let spans = store.get_trace("t1").expect("queries");
    let content = &spans[0].events[0].attributes["content"];
    let reference = content["$payload"].as_str().expect("payload ref");
    assert!(reference.starts_with("sha256/"), "{content}");
    assert_eq!(content["bytes"], 10_000);
    assert_eq!(
        content["preview"].as_str().expect("preview").len(),
        256,
        "preview keeps the head of the text"
    );
    // The bytes come back through the payload API.
    let bytes = store.payload(reference).expect("loads").expect("exists");
    assert_eq!(bytes.len(), 10_000);
    assert!(bytes.iter().all(|&byte| byte == b'P'));

    // Dedup: exactly one payload file exists for the two identical texts.
    let payload_files: Vec<_> = walk(&dir.join("payloads"));
    assert_eq!(payload_files.len(), 1, "{payload_files:?}");

    // Inline content untouched.
    let spans = store.get_trace("t3").expect("queries");
    assert_eq!(spans[0].events[0].attributes["content"], "tiny");
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn annotations_append_query_and_survive_reopen() {
    let dir = test_dir("annotations");
    {
        let store = Store::open(&dir, Config::default()).expect("opens");
        store
            .annotate(Annotation {
                trace_id: "t1".into(),
                span_id: "s1".into(),
                name: "quality".into(),
                value: json!(0.9),
                source: "eval:groundedness".into(),
                comment: String::new(),
                timestamp_ns: 100,
            })
            .expect("annotates");
        store
            .annotate(Annotation {
                trace_id: "t1".into(),
                span_id: String::new(),
                name: "thumbs".into(),
                value: json!("down"),
                source: "human:reviewer".into(),
                comment: "hallucinated the date".into(),
                timestamp_ns: 200,
            })
            .expect("annotates");

        // Empty trace_id / name are invalid.
        assert!(store
            .annotate(Annotation {
                trace_id: String::new(),
                span_id: String::new(),
                name: "x".into(),
                value: json!(1),
                source: String::new(),
                comment: String::new(),
                timestamp_ns: 0,
            })
            .is_err());

        let all = store.annotations("t1", None, None).expect("queries");
        assert_eq!(all.len(), 2);
        let span_only = store.annotations("t1", Some("s1"), None).expect("queries");
        assert_eq!(span_only.len(), 1);
        assert_eq!(span_only[0].name, "quality");
        let by_name = store
            .annotations("t1", None, Some("thumbs"))
            .expect("queries");
        assert_eq!(by_name[0].value, json!("down"));
    }
    // Durable across reopen.
    let store = Store::open(&dir, Config::default()).expect("reopens");
    let all = store.annotations("t1", None, None).expect("queries");
    assert_eq!(all.len(), 2, "annotations survive reopen");
}

// ---------------------------------------------------------------- server

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
            .arg("--payload-threshold-bytes")
            .arg("1024")
            .env_remove("TRAZA_TOKENS")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns");
        let stderr = child.stderr.take().expect("stderr");
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines.next().expect("port line").expect("read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn raw(&self, method: &str, target: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
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
        let body_len = body.map_or(0, <[u8]>::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = body {
            stream.write_all(bytes).expect("body");
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads");
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header end");
        let status = std::str::from_utf8(&response[..split])
            .ok()
            .and_then(|head| head.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status");
        (status, response[split + 4..].to_vec())
    }

    fn json(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let (status, bytes) = self.raw(method, target, encoded.as_deref());
        let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, payload)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn server_round_trips_payloads_annotations_and_export() {
    let dir = test_dir("server");
    let server = Server::spawn(&dir);

    // Ingest one span with an oversized prompt.
    let big = "Q".repeat(50_000);
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "t-wire", "span_id": "s1", "name": "llm.completion",
            "service": "agent", "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
            "events": [{"name": "llm.prompt", "timestamp_ns": 1_000u64,
                         "attributes": {"content": big}}],
        }])),
    );
    assert_eq!(status, 200);

    // The trace returns a reference; the payload endpoint returns the bytes.
    let (status, body) = server.json("GET", "/v1/traces/t-wire", None);
    assert_eq!(status, 200);
    let reference = body["spans"][0]["events"][0]["attributes"]["content"]["$payload"]
        .as_str()
        .expect("ref")
        .to_owned();
    let (status, bytes) = server.raw("GET", &format!("/v1/payloads/{reference}"), None);
    assert_eq!(status, 200);
    assert_eq!(bytes.len(), 50_000);
    let (status, _) = server.raw("GET", "/v1/payloads/sha256/deadbeef", None);
    assert_eq!(status, 404);
    // Traversal-shaped references are refused as not-found, never served.
    let (status, _) = server.raw("GET", "/v1/payloads/sha256/../../LOCK", None);
    assert_eq!(status, 404);

    // Annotate the span; the trace view carries it.
    let (status, _) = server.json(
        "POST",
        "/v1/annotations",
        Some(&json!({
            "trace_id": "t-wire", "span_id": "s1", "name": "thumbs",
            "value": "up", "source": "human:qa"
        })),
    );
    assert_eq!(status, 200);
    let (status, body) = server.json("GET", "/v1/annotations?trace_id=t-wire", None);
    assert_eq!(status, 200);
    assert_eq!(body["annotations"][0]["name"], "thumbs");
    assert!(
        body["annotations"][0]["timestamp_ns"].as_u64().unwrap_or(0) > 0,
        "server stamps the time when the client does not"
    );
    let (status, body) = server.json("GET", "/v1/traces/t-wire", None);
    assert_eq!(status, 200);
    assert_eq!(body["annotations"][0]["value"], "up");

    // Missing trace_id is a 400; unknown params rejected.
    let (status, _) = server.json("GET", "/v1/annotations", None);
    assert_eq!(status, 400);
    let (status, _) = server.json("GET", "/v1/annotations?trace_id=t&x=1", None);
    assert_eq!(status, 400);

    // Export: NDJSON, one span per line, refs preserved.
    let (status, bytes) = server.raw("GET", "/v1/export?service=agent", None);
    assert_eq!(status, 200);
    let text = String::from_utf8(bytes).expect("utf8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let exported: Value = serde_json::from_str(lines[0]).expect("line json");
    assert_eq!(exported["trace_id"], "t-wire");
    assert!(exported["events"][0]["attributes"]["content"]["$payload"]
        .as_str()
        .is_some());
}
