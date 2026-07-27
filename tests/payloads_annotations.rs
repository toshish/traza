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
            durability: traza::Durability::Buffered,
            compaction: None,
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

fn decode_chunked(bytes: &[u8]) -> (Vec<u8>, String) {
    let mut rest = bytes;
    let mut decoded = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk size line");
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..line_end])
                .expect("chunk size utf8")
                .split(';')
                .next()
                .unwrap_or_default(),
            16,
        )
        .expect("chunk size");
        rest = &rest[line_end + 2..];
        if size == 0 {
            return (
                decoded,
                String::from_utf8(rest.to_vec()).expect("trailers utf8"),
            );
        }
        assert!(rest.len() >= size + 2, "complete chunk");
        decoded.extend_from_slice(&rest[..size]);
        assert_eq!(&rest[size..size + 2], b"\r\n");
        rest = &rest[size + 2..];
    }
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

#[test]
fn annotation_replay_rejects_corrupt_middle_records_but_tolerates_a_torn_tail() {
    let valid = Annotation {
        trace_id: "trace".into(),
        span_id: "span".into(),
        name: "quality".into(),
        value: json!(1),
        source: String::new(),
        comment: String::new(),
        timestamp_ns: 1,
    };
    let encoded = serde_json::to_string(&valid).expect("encodes");

    let corrupt = test_dir("annotation-corrupt-middle");
    std::fs::write(
        corrupt.join("annotations.jsonl"),
        format!("{encoded}\nnot-json\n{encoded}\n"),
    )
    .expect("writes corrupt log");
    assert!(
        Store::open(&corrupt, Config::default()).is_err(),
        "newline-terminated middle corruption must fail loudly"
    );

    let torn = test_dir("annotation-torn-tail");
    std::fs::write(
        torn.join("annotations.jsonl"),
        format!("{encoded}\n{{\"trace_id\":\"incomplete"),
    )
    .expect("writes torn log");
    let store = Store::open(&torn, Config::default()).expect("ignores torn final append");
    assert_eq!(store.annotations("trace", None, None).unwrap().len(), 1);
    let mut second = valid.clone();
    second.span_id = "second".into();
    store.annotate(second).expect("appends after recovery");
    drop(store);
    let store = Store::open(&torn, Config::default()).expect("reopens healed log");
    assert_eq!(
        store.annotations("trace", None, None).unwrap().len(),
        2,
        "torn bytes were truncated before the next append"
    );

    let missing_newline = test_dir("annotation-missing-newline");
    std::fs::write(missing_newline.join("annotations.jsonl"), &encoded)
        .expect("writes complete unterminated record");
    let store = Store::open(&missing_newline, Config::default()).expect("accepts complete record");
    let mut second = valid.clone();
    second.span_id = "second".into();
    store
        .annotate(second)
        .expect("appends after delimiter repair");
    drop(store);
    let store = Store::open(&missing_newline, Config::default()).expect("reopens repaired log");
    assert_eq!(
        store.annotations("trace", None, None).unwrap().len(),
        2,
        "missing newline was restored before the next append"
    );
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
        let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
        let body = &response[split + 4..];
        let decoded = if head.contains("transfer-encoding: chunked") {
            decode_chunked(body).0
        } else {
            body.to_vec()
        };
        (status, decoded)
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

    // An unfiltered query is a cross-trace read, not an error: scores are
    // recorded per trace but read as a population. Unknown params are still
    // rejected — a typo must fail loudly rather than widen the result.
    let (status, body) = server.json("GET", "/v1/annotations", None);
    assert_eq!(status, 200, "{body}");
    assert!(
        !body["annotations"]
            .as_array()
            .expect("annotations")
            .is_empty(),
        "an unfiltered query returns every annotation: {body}"
    );
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

#[test]
fn reingested_payload_survives_ttl_compaction() {
    // Found in review: content addressing dedupes identical payloads to one
    // file WITHOUT refreshing its mtime, while the sweep deleted by mtime
    // alone — a fresh span re-referencing old content kept the span but
    // lost its payload. The sweep must honor live references.
    let dir = test_dir("ttl-live");
    let store = Store::open(
        &dir,
        Config {
            ttl_seconds: Some(1),
            payload_threshold: Some(64),
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");

    let shared = "S".repeat(5_000); // referenced by old AND new span
    let doomed = "D".repeat(5_000); // referenced by the old span only
    let mut old_shared = span_with_content("t-old", "s1", &shared);
    old_shared.start_time_ns = 1_000;
    old_shared.end_time_ns = 2_000;
    let mut old_doomed = span_with_content("t-old", "s2", &doomed);
    old_doomed.start_time_ns = 1_000;
    old_doomed.end_time_ns = 2_000;
    store.ingest(old_shared).expect("ingests");
    store.ingest(old_doomed).expect("ingests");
    store.flush().expect("seals the doomed segment");

    // Let the TTL pass, then a FRESH span re-references the shared content
    // (dedup hit: same file, stale mtime).
    std::thread::sleep(Duration::from_millis(1_200));
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut fresh = span_with_content("t-new", "s1", &shared);
    fresh.start_time_ns = now_ns;
    fresh.end_time_ns = now_ns + 1_000;
    store.ingest(fresh).expect("ingests");
    store.flush().expect("seals the fresh segment");

    // Reopen before compacting: the in-memory touch registry starts empty,
    // so survival must come from the LIVE-REFERENCE protection alone (and
    // the orphan is not shielded by its recent touch).
    drop(store);
    let store = Store::open(
        &dir,
        Config {
            ttl_seconds: Some(1),
            payload_threshold: Some(64),
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("reopens");
    store.compact_expired().expect("compacts");

    // The fresh span survived AND kept its payload bytes.
    let spans = store.get_trace("t-new").expect("queries");
    assert_eq!(spans.len(), 1, "fresh span survives the TTL");
    let reference = spans[0].events[0].attributes["content"]["$payload"]
        .as_str()
        .expect("ref")
        .to_owned();
    let bytes = store
        .payload(&reference)
        .expect("loads")
        .expect("payload referenced by a live span must survive the sweep");
    assert_eq!(bytes.len(), 5_000);

    // The truly orphaned payload is gone with its span.
    let doomed_hash = traza::payload::sha256_hex(doomed.as_bytes());
    assert!(
        store
            .payload(&format!("sha256/{doomed_hash}"))
            .expect("loads")
            .is_none(),
        "unreferenced expired payload must be swept"
    );
}

#[test]
fn replaced_spans_count_once_in_rollups() {
    // Found in review: cached rollups summed every PHYSICAL segment copy of
    // a re-ingested span — 2 calls / 30 tokens / $0.30 where the visible
    // truth (primary key, last write wins) was 1 call / 20 tokens / $0.20.
    use traza::analytics::LlmGroupBy;
    let dir = test_dir("replace-count");
    let store = Store::open(&dir, Config::default()).expect("opens");

    let make = |tokens: u64, cost: f64| -> traza::Span {
        serde_json::from_value(json!({
            "trace_id": "t1", "span_id": "s1", "name": "llm.completion",
            "service": "agent", "start_time_ns": 1_000, "end_time_ns": 2_000,
            "attributes": {"session.id": "sess", "llm.model": "m",
                            "llm.prompt_tokens": tokens, "llm.completion_tokens": 0,
                            "llm.cost_usd": cost}
        }))
        .expect("span")
    };

    // Version 1 sealed into a segment; version 2 sealed into a LATER segment.
    store.ingest(make(10, 0.10)).expect("ingests v1");
    store.flush().expect("seals v1");
    store.ingest(make(20, 0.20)).expect("ingests v2");
    store.flush().expect("seals v2");

    for _ in 0..2 {
        // Twice: the second pass exercises the cached-rollup path.
        let sessions = store
            .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
            .expect("lists");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].llm_calls, 1, "one logical span: {sessions:?}");
        assert_eq!(sessions[0].span_count, 1);
        assert_eq!(sessions[0].total_tokens, 20, "last write wins");
        assert!((sessions[0].cost_usd - 0.20).abs() < 1e-9);

        let rows = store
            .llm_aggregate(LlmGroupBy::Model, None, None)
            .expect("aggregates");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].llm_calls, 1, "{rows:?}");
        assert_eq!(rows[0].total_tokens, 20);
    }

    // A third version still in the BUFFER also wins over both segments.
    store.ingest(make(50, 0.50)).expect("ingests v3 buffered");
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("lists");
    assert_eq!(sessions[0].total_tokens, 50, "buffer supersedes segments");
    assert_eq!(sessions[0].llm_calls, 1);
    let by_day = store
        .llm_aggregate(LlmGroupBy::Day, None, None)
        .expect("aggregates");
    assert_eq!(by_day[0].llm_calls, 1, "{by_day:?}");
}

#[test]
fn concurrent_identical_payload_ingest_all_succeed() {
    // Found in review: a shared `<hash>.tmp` path let ten simultaneous
    // identical-payload ingests truncate each other's temp file and race
    // the rename — nine successes, one ENOENT. Unique temps fix it.
    use std::sync::Arc;
    let dir = test_dir("concurrent");
    let store = Arc::new(
        Store::open(
            &dir,
            Config {
                payload_threshold: Some(1_024),
                durability: traza::Durability::Buffered,
                compaction: None,
                ..Config::default()
            },
        )
        .expect("opens"),
    );
    let content = Arc::new("C".repeat(1_000_000));
    let mut handles = Vec::new();
    for worker in 0..10 {
        let store = Arc::clone(&store);
        let content = Arc::clone(&content);
        handles.push(std::thread::spawn(move || {
            let span = span_with_content(&format!("t{worker}"), "s1", &content);
            store.ingest(span)
        }));
    }
    for handle in handles {
        handle
            .join()
            .expect("no panic")
            .expect("every concurrent identical ingest must succeed");
    }
    // One deduped file; the bytes are intact.
    let files = walk(&dir.join("payloads"));
    let payload_files: Vec<_> = files
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    assert_eq!(payload_files.len(), 1, "{files:?}");
    let spans = store.get_trace("t0").expect("queries");
    let reference = spans[0].events[0].attributes["content"]["$payload"]
        .as_str()
        .expect("ref")
        .to_owned();
    let bytes = store.payload(&reference).expect("loads").expect("exists");
    assert_eq!(bytes.len(), 1_000_000);
    // And no leftover temp litter.
    assert!(
        files
            .iter()
            .all(|path| !path.to_string_lossy().ends_with(".tmp")),
        "temps must be renamed or cleaned: {files:?}"
    );
}

#[test]
fn export_streams_with_completion_trailers_and_paginates_exactly() {
    // Found in review: export materialized the full result plus a full
    // NDJSON buffer, defeating larger-than-RAM. A complete full-key cursor now
    // holds pages constant even for a timestamp run wider than one page.
    let dir = test_dir("export-stream");
    let server = Server::spawn(&dir);

    // 6,000 spans sharing one timestamp (page size is 4,096), plus one
    // replaced span that must appear exactly once with its final name.
    let mut batch = Vec::new();
    for index in 0..6_000 {
        batch.push(json!({
            "trace_id": format!("t{}", index / 10), "span_id": format!("s{index}"),
            "name": "op", "service": "bulk",
            "start_time_ns": 42_000u64, "end_time_ns": 43_000u64,
        }));
    }
    let (status, _) = server.json("POST", "/v1/spans", Some(&json!(batch)));
    assert_eq!(status, 200);
    for name in ["first", "final"] {
        let (status, _) = server.json(
            "POST",
            "/v1/spans",
            Some(&json!([{
                "trace_id": "t-replaced", "span_id": "s1", "name": name,
                "service": "bulk", "start_time_ns": 41_000u64, "end_time_ns": 41_500u64,
            }])),
        );
        assert_eq!(status, 200);
    }

    let (status, bytes) = server.raw("GET", "/v1/export?service=bulk", None);
    assert_eq!(status, 200);
    let text = String::from_utf8(bytes).expect("utf8");
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(lines.len(), 6_001, "every span exactly once across pages");
    let replaced: Vec<Value> = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|span| span["trace_id"] == "t-replaced")
        .collect();
    assert_eq!(replaced.len(), 1, "replaced span appears once");
    assert_eq!(replaced[0]["name"], "final", "last write wins in export");

    // Chunked framing preserves NDJSON while trailers prove completion.
    {
        use std::io::{Read, Write};
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", server.port)).expect("connects");
        write!(
            stream,
            "GET /v1/export?service=bulk&limit=5 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("reads");
        let text = String::from_utf8_lossy(&response);
        let head = text
            .split("\r\n\r\n")
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            !head.contains("content-length"),
            "chunked export must not declare content length: {head}"
        );
        assert!(head.contains("transfer-encoding: chunked"), "{head}");
        assert!(
            head.contains("trailer: x-traza-export-complete, x-traza-export-count"),
            "{head}"
        );
        let body = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|split| &response[split + 4..])
            .expect("body");
        let (decoded, trailers) = decode_chunked(body);
        let body_lines = String::from_utf8(decoded)
            .expect("body utf8")
            .lines()
            .filter(|line| !line.is_empty())
            .count();
        assert_eq!(body_lines, 5, "user limit caps the stream");
        assert!(
            trailers
                .to_ascii_lowercase()
                .contains("x-traza-export-complete: true"),
            "{trailers}"
        );
        assert!(
            trailers
                .to_ascii_lowercase()
                .contains("x-traza-export-count: 5"),
            "{trailers}"
        );
    }
}

#[test]
fn export_storage_failure_is_explicit_in_trailers() {
    let dir = test_dir("export-failure");
    let server = Server::spawn(&dir);
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "trace", "span_id": "span", "name": "op",
            "service": "broken", "start_time_ns": 1u64, "end_time_ns": 2u64
        }])),
    );
    assert_eq!(status, 200);
    assert_eq!(server.json("POST", "/v1/flush", None).0, 200);
    let segment = walk(&dir)
        .into_iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .expect("segment");
    std::fs::OpenOptions::new()
        .write(true)
        .open(segment)
        .expect("opens segment")
        .set_len(0)
        .expect("truncates segment");

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connects");
    write!(
        stream,
        "GET /v1/export?service=broken HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .expect("writes");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("reads");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header");
    assert!(
        String::from_utf8_lossy(&response[..split]).starts_with("HTTP/1.1 200"),
        "headers precede the storage failure"
    );
    let (decoded, trailers) = decode_chunked(&response[split + 4..]);
    assert!(decoded.is_empty());
    let trailers = trailers.to_ascii_lowercase();
    assert!(
        trailers.contains("x-traza-export-complete: false"),
        "{trailers}"
    );
    assert!(trailers.contains("x-traza-export-count: 0"), "{trailers}");
}

#[test]
fn export_keeps_equal_timestamp_rows_from_different_segments() {
    // Limited queries used to break equal-start ties by source order while
    // the export cursor assumed the engine's full (start, end, trace, span)
    // order. Persisting z before a made the first row advance the cursor past
    // a, so a was silently omitted from the stream.
    let dir = test_dir("export-cross-segment-tie");
    let server = Server::spawn(&dir);

    for (trace_id, name) in [("z-trace", "z"), ("a-trace", "a")] {
        let (status, _) = server.json(
            "POST",
            "/v1/spans",
            Some(&json!([{
                "trace_id": trace_id,
                "span_id": "s",
                "name": name,
                "service": "tie",
                "start_time_ns": 42_000u64,
                "end_time_ns": 43_000u64
            }])),
        );
        assert_eq!(status, 200);
        let (status, _) = server.json("POST", "/v1/flush", None);
        assert_eq!(status, 200);
    }

    let (status, bytes) = server.raw("GET", "/v1/export?service=tie", None);
    assert_eq!(status, 200);
    let trace_ids: Vec<String> = String::from_utf8(bytes)
        .expect("utf8")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("span JSON")["trace_id"]
                .as_str()
                .expect("trace id")
                .to_owned()
        })
        .collect();
    assert_eq!(
        trace_ids,
        vec!["a-trace", "z-trace"],
        "every cross-segment tie must be exported once in total order"
    );
}
