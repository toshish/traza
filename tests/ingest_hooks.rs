//! The ingest hooks are wired in without changing what ingest does.
//!
//! Every batch now passes an admission check before the erasure gate and an
//! observer after acknowledgement, and the observer's signals ride back on
//! `Admission::signals`. These batches stay far from every default fuse
//! limit, so the proof is that both ingest surfaces answer exactly as they
//! did — the response bodies pinned whole, so a stray `fuses` field fails
//! here — and that the engine reports an empty signal list on every
//! admission path it has (`tests/guard.rs` covers the batches that do raise
//! one). The
//! last test exercises the shared process harness itself, which every new
//! integration test builds on.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{header, Server};
use serde_json::{json, Map, Value};
use traza::{Config, Span, Store};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "traza-hooks-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn span(trace: &str, id: &str, attributes: Value) -> Span {
    Span {
        trace_id: trace.into(),
        span_id: id.into(),
        tenant: String::new(),
        parent_span_id: None,
        name: "op".into(),
        start_time_ns: 1_000,
        end_time_ns: 2_000,
        status: "ok".into(),
        service: "svc".into(),
        attributes: attributes.as_object().expect("object").clone(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

#[test]
fn every_engine_admission_carries_an_empty_signal_list() {
    let dir = TempDir::new("engine");
    let store = Store::open(&dir.0, Config::default()).expect("opens");

    let admission = store
        .ingest_batch(vec![
            span("t1", "a", json!({"session.id": "run-1", "llm.model": "m"})),
            span("t1", "b", json!({"gen_ai.tool.name": "search"})),
        ])
        .expect("ingests");
    assert_eq!(admission.accepted, 2);
    assert_eq!(admission.suppressed, 0);
    assert!(admission.signals.is_empty(), "{:?}", admission.signals);

    // The single-span path and the empty-batch shortcut agree.
    let single = store.ingest(span("t2", "c", json!({}))).expect("ingests");
    assert_eq!((single.accepted, single.signals.len()), (1, 0));
    let empty = store.ingest_batch(Vec::new()).expect("empty batch");
    assert_eq!(empty, traza::Admission::default());

    // A batch the engine refuses is refused whole, hooks or not.
    let refused = store.ingest_batch(vec![span("t3", "ok", json!({})), span("t3", "", json!({}))]);
    assert!(
        matches!(refused, Err(traza::Error::InvalidSpan(_))),
        "{refused:?}"
    );
    assert!(store.get_trace("t3").expect("reads").is_empty());

    // And what was admitted reads back as a run. The trace form is the
    // whole trace; the session form only the spans carrying the session key
    // (`t1/b` carries none).
    let run = store
        .get_run_in(Some(""), "trace:t1", 10)
        .expect("resolves");
    assert_eq!(run.spans.len(), 2, "{run:?}");
    let session = store.get_run_in(Some(""), "run-1", 10).expect("resolves");
    assert_eq!(session.spans.len(), 1, "{session:?}");
    assert_eq!(session.spans[0].span_id, "a");
}

#[test]
fn both_ingest_surfaces_answer_exactly_as_before() {
    let server = Server::spawn(&[]);

    let (status, body) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([
            {"trace_id": "t-hooks", "span_id": "s1", "name": "chat", "service": "agent",
             "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
             "attributes": {"gen_ai.operation.name": "chat", "session.id": "run-7"}},
            {"trace_id": "t-hooks", "span_id": "s2", "name": "search", "service": "agent",
             "start_time_ns": 3_000u64, "end_time_ns": 4_000u64,
             "attributes": {"gen_ai.operation.name": "execute_tool"}},
        ])),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"accepted": 2, "durability": "wal"}));

    let (status, body) = server.json(
        "POST",
        "/v1/traces",
        Some(&json!({"resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "agent"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "spanId": "bbbbbbbbbbbbbbbb",
                "name": "llm",
                "startTimeUnixNano": "1700000000000000000",
                "endTimeUnixNano": "1700000001000000000",
                "attributes": [
                    {"key": "openinference.span.kind", "value": {"stringValue": "LLM"}}
                ]
            }]}]
        }]})),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"partialSuccess": {}}));

    let (status, body) = server.json("GET", "/v1/traces/t-hooks", None, &[]);
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(2), "{body}");
}

#[test]
fn the_harness_restarts_on_the_same_data_and_keeps_its_environment() {
    let mut server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "rw:harness-token")]);
    let bearer = [("Authorization", "Bearer harness-token")];
    let one = json!([{"trace_id": "t-restart", "span_id": "s1", "name": "op",
                      "service": "svc", "start_time_ns": 1u64, "end_time_ns": 2u64}]);

    // The environment reached the child: no bearer, no entry.
    let (status, _) = server.json("POST", "/v1/spans", Some(&one), &[]);
    assert_eq!(status, 401);
    let (status, body) = server.json("POST", "/v1/spans", Some(&one), &bearer);
    assert_eq!(status, 200, "{body}");
    assert!(server.data_dir.is_dir());
    assert!(
        server
            .stderr()
            .contains("traza-server listening on 127.0.0.1:"),
        "stderr is captured: {}",
        server.stderr()
    );

    // A crash restart keeps the data directory, the arguments and the
    // environment; the acknowledged span survives through the log.
    server.restart();
    let (status, _) = server.json("GET", "/v1/traces/t-restart", None, &[]);
    assert_eq!(status, 401, "the restarted server still enforces auth");
    let (status, body) = server.json("GET", "/v1/traces/t-restart", None, &bearer);
    assert_eq!(status, 200);
    assert_eq!(body["spans"][0]["span_id"], "s1", "{body}");

    // Chunked responses are de-chunked and their trailers surfaced.
    let (status, headers, text) = server.request(
        "GET",
        "/v1/export?service=svc",
        None,
        &[("Authorization", "Bearer harness-token")],
    );
    assert_eq!(status, 200, "{text}");
    assert_eq!(header(&headers, "x-traza-export-complete"), Some("true"));
    assert_eq!(header(&headers, "X-Traza-Export-Count"), Some("1"));
    let line: Value = serde_json::from_str(text.trim()).expect("one NDJSON line");
    assert_eq!(line["span_id"], "s1");
    assert!(server
        .url("/v1/stats")
        .ends_with(&format!(":{}/v1/stats", server.port)));

    let data_dir = server.data_dir.clone();
    drop(server);
    assert!(!data_dir.exists(), "drop removes the data directory");
}
