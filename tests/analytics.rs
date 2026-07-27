//! Sessions and LLM aggregation acceptance: derived views must be exact
//! across the write buffer, sealed segments, window boundaries, and reopen.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::analytics::LlmGroupBy;
use traza::{Config, Span, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-analytics-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

const DAY_NS: u64 = 86_400_000_000_000;

/// An LLM call span: session + model + token/cost attributes.
#[allow(clippy::too_many_arguments)]
fn llm_span(
    trace_id: &str,
    span_id: &str,
    session: &str,
    model: &str,
    start_ns: u64,
    prompt: u64,
    completion: u64,
    cost: f64,
) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace_id, "span_id": span_id, "name": "llm.completion",
        "service": "agent", "start_time_ns": start_ns, "end_time_ns": start_ns + 5_000_000,
        "attributes": {
            "session.id": session,
            "llm.model": model,
            "llm.prompt_tokens": prompt,
            "llm.completion_tokens": completion,
            "llm.cost_usd": cost,
        }
    }))
    .expect("span")
}

/// A plain span (no session, no LLM attributes).
fn plain_span(trace_id: &str, span_id: &str, service: &str, start_ns: u64, status: &str) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace_id, "span_id": span_id, "name": "op", "service": service,
        "start_time_ns": start_ns, "end_time_ns": start_ns + 1_000, "status": status,
    }))
    .expect("span")
}

#[test]
fn sessions_aggregate_across_buffer_segments_and_reopen() {
    let dir = test_dir("sessions");
    // Pinned to `buffered`: this test documents the LOSSY contract, where an
    // unflushed span is volatile. The recoverable default is covered below
    // and, end to end through SIGKILL, in tests/durability.rs.
    let buffered = Config {
        durability: traza::Durability::Buffered,
        compaction: None,
        ..Config::default()
    };
    {
        let store = Store::open(&dir, buffered.clone()).expect("opens");
        // Session A: two traces, one persisted and one buffered.
        store
            .ingest(llm_span(
                "t1", "s1", "sess-a", "m-fast", 1_000, 100, 50, 0.01,
            ))
            .expect("ingests");
        store
            .ingest(llm_span(
                "t1", "s2", "sess-a", "m-smart", 2_000, 200, 100, 0.20,
            ))
            .expect("ingests");
        store.flush().expect("seals the first segment");
        store
            .ingest(llm_span(
                "t2", "s1", "sess-a", "m-fast", 3_000, 10, 5, 0.001,
            ))
            .expect("ingests buffered");
        // Session B: a single errored call.
        let mut errored = llm_span("t3", "s1", "sess-b", "m-fast", 4_000, 1, 1, 0.0005);
        errored.status = "error".into();
        store.ingest(errored).expect("ingests");
        // Sessionless noise never joins a session.
        store
            .ingest(plain_span("t4", "s1", "web", 5_000, "ok"))
            .expect("ingests");

        let sessions = store.sessions(None, None, 10).expect("lists");
        assert_eq!(sessions.len(), 2, "two sessions: {sessions:?}");
        // Most recent activity first: sess-b ends later.
        assert_eq!(sessions[0].session_id, "sess-b");
        assert_eq!(sessions[0].error_count, 1);
        let a = &sessions[1];
        assert_eq!(a.session_id, "sess-a");
        assert_eq!(a.span_count, 3);
        assert_eq!(a.llm_calls, 3);
        assert_eq!(a.trace_count, 2, "distinct traces across segment+buffer");
        assert_eq!(a.prompt_tokens, 310);
        assert_eq!(a.completion_tokens, 155);
        assert_eq!(a.total_tokens, 465, "derived prompt+completion totals");
        assert!((a.cost_usd - 0.211).abs() < 1e-9, "cost: {}", a.cost_usd);
    }
    // A cold reopen rebuilds rollups from what was DURABLE: only the sealed
    // segment's two sess-a spans survive (buffered spans are volatile until
    // flush, by design).
    let store = Store::open(&dir, buffered).expect("reopens");
    let sessions = store.sessions(None, None, 10).expect("lists after reopen");
    assert_eq!(
        sessions.len(),
        1,
        "only the persisted session: {sessions:?}"
    );
    assert_eq!(sessions[0].session_id, "sess-a");
    assert_eq!(sessions[0].span_count, 2);
    assert_eq!(sessions[0].total_tokens, 450);
}

#[test]
fn the_default_mode_recovers_unflushed_sessions_across_reopen() {
    // The same shape as the test above, under the DEFAULT durability: what was
    // volatile in `buffered` is recovered from the log, so a session is whole
    // after a restart even though it was never flushed.
    let dir = test_dir("sessions-wal");
    {
        let store = Store::open(&dir, Config::default()).expect("opens");
        store
            .ingest(llm_span(
                "t1", "s1", "sess-a", "m-fast", 1_000, 100, 50, 0.01,
            ))
            .expect("ingests");
        store
            .ingest(llm_span(
                "t1", "s2", "sess-a", "m-smart", 2_000, 200, 100, 0.20,
            ))
            .expect("ingests");
        store.flush().expect("seals the first segment");
        // Acknowledged but never flushed: only the log protects these.
        store
            .ingest(llm_span(
                "t2", "s1", "sess-a", "m-fast", 3_000, 10, 5, 0.001,
            ))
            .expect("ingests");
    }
    let store = Store::open(&dir, Config::default()).expect("reopens");
    let sessions = store.sessions(None, None, 10).expect("lists after reopen");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let recovered = &sessions[0];
    assert_eq!(recovered.session_id, "sess-a");
    assert_eq!(recovered.span_count, 3, "the unflushed span is recovered");
    assert_eq!(recovered.trace_count, 2);
    assert_eq!(recovered.total_tokens, 465, "its tokens count again");
}

#[test]
fn session_detail_breaks_down_per_trace() {
    let dir = test_dir("detail");
    let store = Store::open(&dir, Config::default()).expect("opens");
    store
        .ingest(llm_span("t1", "s1", "sess", "m", 1_000, 10, 10, 0.01))
        .expect("ingests");
    store
        .ingest(llm_span("t1", "s2", "sess", "m", 2_000, 10, 10, 0.01))
        .expect("ingests");
    store
        .ingest(llm_span("t2", "s1", "sess", "m", 9_000, 5, 5, 0.02))
        .expect("ingests");

    let detail = store.session("sess").expect("queries").expect("exists");
    assert_eq!(detail.summary.span_count, 3);
    assert_eq!(detail.traces.len(), 2);
    assert_eq!(detail.traces[0].trace_id, "t1", "ordered by first activity");
    assert_eq!(detail.traces[0].span_count, 2);
    assert_eq!(detail.traces[0].total_tokens, 40);
    assert_eq!(detail.traces[1].trace_id, "t2");
    assert!((detail.traces[1].cost_usd - 0.02).abs() < 1e-9);

    assert!(store.session("nope").expect("queries").is_none());
}

#[test]
fn llm_aggregate_groups_and_windows_exactly() {
    let dir = test_dir("aggregate");
    let store = Store::open(&dir, Config::default()).expect("opens");
    let day0 = 0;
    let day1 = DAY_NS;
    // Day 0, persisted.
    store
        .ingest(llm_span(
            "t1",
            "s1",
            "sess-1",
            "m-fast",
            day0 + 1_000,
            100,
            50,
            0.01,
        ))
        .expect("ingests");
    store
        .ingest(llm_span(
            "t1",
            "s2",
            "sess-1",
            "m-smart",
            day0 + 2_000,
            1_000,
            400,
            0.50,
        ))
        .expect("ingests");
    store.flush().expect("seals day-0 segment");
    // Day 1, buffered.
    store
        .ingest(llm_span(
            "t2",
            "s1",
            "sess-2",
            "m-fast",
            day1 + 1_000,
            30,
            20,
            0.003,
        ))
        .expect("ingests");

    // Group by model across everything.
    let by_model = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(by_model.len(), 2);
    assert_eq!(by_model[0].key, "m-smart", "sorted by cost: {by_model:?}");
    assert_eq!(by_model[0].total_tokens, 1_400);
    let fast = &by_model[1];
    assert_eq!(fast.key, "m-fast");
    assert_eq!(fast.llm_calls, 2);
    assert_eq!(fast.total_tokens, 200);

    // Group by day: two buckets with exact membership.
    let by_day = store
        .llm_aggregate(LlmGroupBy::Day, None, None)
        .expect("aggregates");
    assert_eq!(by_day.len(), 2);
    let day0_row = by_day
        .iter()
        .find(|row| row.key == "1970-01-01")
        .expect("day0");
    assert_eq!(day0_row.llm_calls, 2);
    let day1_row = by_day
        .iter()
        .find(|row| row.key == "1970-01-02")
        .expect("day1");
    assert_eq!(day1_row.llm_calls, 1);

    // A window that splits the persisted segment: only the second span
    // qualifies — boundary segments must be decoded exactly, not
    // rollup-approximated.
    let windowed = store
        .llm_aggregate(LlmGroupBy::Model, Some(day0 + 1_500), None)
        .expect("aggregates");
    assert_eq!(
        windowed.iter().map(|row| row.llm_calls).sum::<usize>(),
        2,
        "window must exclude the day-0 first span exactly: {windowed:?}"
    );
    assert!(windowed
        .iter()
        .all(|row| row.key != "m-fast" || row.llm_calls == 1));

    // Session grouping keys by the session attribute.
    let by_session = store
        .llm_aggregate(LlmGroupBy::Session, None, None)
        .expect("aggregates");
    assert_eq!(by_session.len(), 2);

    // Service grouping counts non-LLM spans too.
    store
        .ingest(plain_span("t9", "s1", "agent", day1 + 5_000, "ok"))
        .expect("ingests");
    let by_service = store
        .llm_aggregate(LlmGroupBy::Service, None, None)
        .expect("aggregates");
    let agent = by_service
        .iter()
        .find(|row| row.key == "agent")
        .expect("agent row");
    assert_eq!(agent.spans, 4);
    assert_eq!(agent.llm_calls, 3);
}

#[test]
fn numeric_strings_and_explicit_totals_are_honored() {
    // OTLP and loose producers stringify counters; explicit llm.total_tokens
    // must win over prompt+completion when both are present.
    let dir = test_dir("coerce");
    let store = Store::open(&dir, Config::default()).expect("opens");
    let span: Span = serde_json::from_value(json!({
        "trace_id": "t", "span_id": "s", "name": "llm.completion", "service": "agent",
        "start_time_ns": 1_000, "end_time_ns": 2_000,
        "attributes": {
            "session.id": "sess",
            "llm.model": "m",
            "llm.prompt_tokens": "120",
            "llm.completion_tokens": "30",
            "llm.total_tokens": "180",
            "llm.cost_usd": "0.25",
        }
    }))
    .expect("span");
    store.ingest(span).expect("ingests");
    let sessions = store.sessions(None, None, 10).expect("lists");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].prompt_tokens, 120);
    assert_eq!(sessions[0].completion_tokens, 30);
    assert_eq!(sessions[0].total_tokens, 180, "explicit total wins");
    assert!((sessions[0].cost_usd - 0.25).abs() < 1e-9);
}

#[test]
fn aggregate_overflow_saturates_and_non_finite_cost_is_ignored() {
    let dir = test_dir("overflow");
    let store = Store::open(&dir, Config::default()).expect("opens");
    for index in 0..2 {
        let mut span = llm_span(
            &format!("trace-{index}"),
            "span",
            "session",
            "model",
            index,
            0,
            0,
            0.0,
        );
        span.attributes
            .insert("llm.total_tokens".into(), json!(u64::MAX));
        span.attributes.insert("llm.cost_usd".into(), json!("NaN"));
        store.ingest(span).expect("ingests");
    }

    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates without panic");
    assert_eq!(rows[0].total_tokens, u64::MAX, "counter saturates");
    assert_eq!(rows[0].cost_usd, 0.0, "non-finite cost is ignored");
}

#[test]
fn rollup_cache_survives_compaction_supersede() {
    // Expiring a segment must not leave its rollup haunting the answers.
    let dir = test_dir("supersede");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 10_000,
            ttl_seconds: Some(1),
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            content_index: true,
            tail_ring_spans: traza::DEFAULT_TAIL_RING_SPANS,
            flush_wal_bytes: None,
        },
    )
    .expect("opens");
    // An old span, sealed, then aggregated (populating the cache).
    store
        .ingest(llm_span(
            "t-old", "s1", "sess-old", "m", 1_000, 10, 10, 0.01,
        ))
        .expect("ingests");
    store.flush().expect("seals");
    let before = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(before.len(), 1);
    // Everything is far older than the TTL: compaction drops it.
    store.compact_expired().expect("compacts");
    let after = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert!(
        after.is_empty(),
        "expired spans must leave the aggregates: {after:?}"
    );
    let sessions = store.sessions(None, None, 10).expect("lists");
    assert!(sessions.is_empty(), "expired sessions vanish: {sessions:?}");
}

#[test]
fn server_serves_sessions_and_llm_stats() {
    // Thin wire check: the endpoints exist, parse their params, and reject junk.
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::process::{Child, Command, Stdio};

    struct Server {
        child: Child,
        port: u16,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    fn request(port: u16, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 50 => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(20));
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
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        (status, payload)
    }

    let dir = test_dir("server");
    let mut child = Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .arg("--data-dir")
        .arg(&dir)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .env_remove("TRAZA_TOKENS")
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns");
    let stderr = child.stderr.take().expect("stderr");
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
    let port = loop {
        let line = lines.next().expect("port line").expect("read");
        if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
            break rest.trim().parse::<u16>().expect("port");
        }
    };
    std::thread::spawn(move || for _ in lines {});
    let server = Server { child, port };

    let (status, _) = request(
        server.port,
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "t1", "span_id": "s1", "name": "llm.completion", "service": "agent",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
            "attributes": {"session.id": "wire-sess", "llm.model": "m-x",
                            "llm.prompt_tokens": 7, "llm.completion_tokens": 3,
                            "llm.cost_usd": 0.001}
        }])),
    );
    assert_eq!(status, 200);

    let (status, body) = request(server.port, "GET", "/v1/sessions", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["sessions"][0]["session_id"], "wire-sess");
    assert_eq!(body["sessions"][0]["total_tokens"], 10);

    let (status, body) = request(server.port, "GET", "/v1/sessions/wire-sess", None);
    assert_eq!(status, 200);
    assert_eq!(body["traces"][0]["trace_id"], "t1");
    let (status, _) = request(server.port, "GET", "/v1/sessions/absent", None);
    assert_eq!(status, 404);

    let (status, body) = request(server.port, "GET", "/v1/stats/llm?group_by=model", None);
    assert_eq!(status, 200);
    assert_eq!(body["rows"][0]["key"], "m-x");
    assert_eq!(body["rows"][0]["llm_calls"], 1);
    let (status, _) = request(server.port, "GET", "/v1/stats/llm?group_by=galaxy", None);
    assert_eq!(status, 400);
    let (status, _) = request(server.port, "GET", "/v1/sessions?surprise=1", None);
    assert_eq!(status, 400);
}
