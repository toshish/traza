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

        let sessions = store
            .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
            .expect("lists");
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
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("lists after reopen");
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
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("lists after reopen");
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
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("lists");
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
            pricing: Default::default(),
            tenant_ttl_seconds: Default::default(),
            flush_spans: 10_000,
            max_buffer_age: None,
            shadow_seal: false,
            ttl_seconds: Some(1),
            payload_threshold: None,
            durability: traza::Durability::Buffered,
            compaction: None,
            wal_commit_window: None,
            content_index: true,
            tail_ring_spans: traza::DEFAULT_TAIL_RING_SPANS,
            tail_ring_bytes: traza::DEFAULT_TAIL_RING_BYTES,
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
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("lists");
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
        // A close delivered as RST once the response is complete is
        // tolerated: the server closes the socket as soon as it answers, and
        // a loaded kernel can turn that into a reset that `read_to_end`
        // reports AFTER handing over every byte (the lesson of
        // `tests/auth.rs`). An incomplete response still panics.
        let mut response = Vec::new();
        if let Err(error) = stream.read_to_end(&mut response) {
            let complete = response
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .and_then(|header_end| {
                    let head = std::str::from_utf8(&response[..header_end]).ok()?;
                    let length = head.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })?;
                    Some(response.len() >= header_end + 4 + length)
                })
                .unwrap_or(false);
            assert!(
                complete,
                "incomplete response after {:?}: {error}",
                error.kind()
            );
        }
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

/// The partially-overlapping-segment path: a window that straddles a segment
/// must decode only the window's slice AND must still honor last-write-wins.
///
/// Both halves are load-bearing and they pull against each other. The fast
/// path skips work using a hash prefilter over the keys of newer segments; the
/// windowed decode skips records outside the window. A prefilter that let a
/// superseded span through would double-count it, and a window search off by
/// one record would silently drop or invent a span. The assertions below pin
/// exact money, so either mistake shows up as a wrong number rather than as a
/// wrong-looking shape.
#[test]
fn partial_windows_stay_exact_across_superseding_segments() {
    let dir = test_dir("partial-window");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ..Config::default()
        },
    )
    .expect("opens");

    // Segment 0 STRADDLES the window below: one span inside, one far outside.
    store
        .ingest(llm_span("t-1", "s-1", "sess", "m", 1_000, 10, 10, 0.10))
        .expect("ingests");
    store
        .ingest(llm_span("t-9", "s-9", "sess", "m", 9_000, 10, 10, 0.90))
        .expect("ingests");
    store.flush().expect("seals");

    // Segment 1: a span outside the window, so the window rules it out whole.
    store
        .ingest(llm_span("t-2", "s-2", "sess", "m", 3_000, 10, 10, 0.20))
        .expect("ingests");
    store.flush().expect("seals");

    // Segment 2 REPLACES the in-window span from segment 0 under the same
    // primary key, at the same start time, with different money.
    store
        .ingest(llm_span("t-1", "s-1", "sess", "m", 1_000, 5, 5, 0.99))
        .expect("ingests");
    store.flush().expect("seals");

    let money = |since: Option<u64>, until: Option<u64>| {
        let rows = store
            .llm_aggregate(LlmGroupBy::Model, since, until)
            .expect("aggregates");
        rows.iter()
            .map(|row| (row.spans, row.total_tokens, row.cost_usd))
            .fold((0, 0, 0.0), |(spans, tokens, cost), row| {
                (spans + row.0, tokens + row.1, cost + row.2)
            })
    };

    // The whole corpus: three live spans, the replaced version gone.
    let (spans, tokens, cost) = money(None, None);
    assert_eq!(spans, 3, "the superseded version must not survive");
    assert_eq!(tokens, 10 + 20 + 20);
    assert!(
        (cost - (0.99 + 0.20 + 0.90)).abs() < 1e-9,
        "whole-corpus cost was {cost}"
    );

    // A narrow window straddling segment 0: only the replaced span qualifies,
    // and only in its NEWEST version.
    let (spans, tokens, cost) = money(Some(900), Some(1_100));
    assert_eq!(spans, 1, "the window holds exactly one live span");
    assert_eq!(tokens, 10, "the newest version's tokens, not the replaced");
    assert!((cost - 0.99).abs() < 1e-9, "windowed cost was {cost}");

    // A window covering the tail of segment 0 only: the out-of-window half of
    // a straddling segment must not leak in.
    let (spans, _, cost) = money(Some(8_000), None);
    assert_eq!(spans, 1);
    assert!((cost - 0.90).abs() < 1e-9, "tail-window cost was {cost}");

    // A window that selects nothing between two populated segments.
    assert_eq!(money(Some(4_000), Some(8_000)).0, 0, "an empty window is 0");

    // Exact bounds are inclusive at both ends.
    assert_eq!(
        money(Some(1_000), Some(1_000)).0,
        1,
        "point window at a span"
    );
    assert_eq!(money(Some(1_001), Some(2_999)).0, 0, "point window between");
}

/// Every fingerprint an aggregation can be judged on, as plain comparable
/// values. A rollup that survives a restart has to reproduce ALL of these —
/// a codec that dropped, say, `llm_duration_ns` would still return plausible
/// cost and token numbers, and nothing in a spot-check would notice.
fn fingerprint(store: &Store) -> (Vec<String>, Vec<String>) {
    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates")
        .iter()
        .map(|row| {
            format!(
                "{}|{}|{}|{}|{}|{}|{:.12}|{}|{}",
                row.key,
                row.spans,
                row.llm_calls,
                row.prompt_tokens,
                row.completion_tokens,
                row.total_tokens,
                row.cost_usd,
                row.error_count,
                row.llm_duration_ns
            )
        })
        .collect();
    let sessions = store
        .sessions(None, None, 100, traza::analytics::SessionOrder::Recent)
        .expect("sessions")
        .iter()
        .map(|session| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{:.12}|{}",
                session.session_id,
                session.session_attribute,
                session.first_start_ns,
                session.last_end_ns,
                session.trace_count,
                session.span_count,
                session.llm_calls,
                session.total_tokens,
                session.cost_usd,
                session.error_count
            )
        })
        .collect();
    (rows, sessions)
}

fn rollup_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rollup"))
        .collect();
    found.sort();
    found
}

fn seeded_store(dir: &PathBuf) -> Store {
    Store::open(
        dir,
        Config {
            flush_spans: 100_000,
            ..Config::default()
        },
    )
    .expect("opens")
}

/// Seals a corpus varied enough that every field of the persisted rollup
/// carries a distinct value: several models, several sessions spanning
/// several traces, errors, and a span whose session arrives under a
/// non-default key so the session-key precedence has to round-trip too.
fn seed(store: &Store) {
    for index in 0..120_u64 {
        let model = ["gpt-4o", "claude-opus-4", "gemini-2.5-pro"][index as usize % 3];
        let mut span = llm_span(
            &format!("t-{}", index / 4),
            &format!("s-{index}"),
            &format!("sess-{}", index / 10),
            model,
            1_000_000 + index * 1_000,
            10 + index,
            5 + index,
            0.001 * index as f64,
        );
        if index % 7 == 0 {
            span.status = "error".to_owned();
        }
        store.ingest(span).expect("ingests");
    }
    // A session arriving under a different recognized key: the persisted
    // rollup stores the key by INDEX, so a bad round-trip regroups sessions.
    let aliased: Span = serde_json::from_value(json!({
        "trace_id": "t-alias", "span_id": "s-alias", "name": "llm.completion",
        "service": "agent", "start_time_ns": 2_000_000, "end_time_ns": 2_500_000,
        "attributes": {
            "gen_ai.conversation.id": "sess-aliased",
            "llm.model": "gpt-4o",
            "llm.prompt_tokens": 7,
            "llm.completion_tokens": 3,
            "llm.cost_usd": 0.25,
        }
    }))
    .expect("span");
    store.ingest(aliased).expect("ingests");
    store.flush().expect("seals");
}

/// The rollup sidecar must reproduce a rebuild EXACTLY, and must be ignored
/// whenever it cannot be trusted.
///
/// The sidecar exists so that the first aggregation after a restart is a file
/// read instead of a decode of the whole corpus. That makes it a cache in
/// front of the only numbers this product reports, so the bar is not "close
/// enough" — it is byte-for-byte the same answer, or don't use it.
#[test]
fn the_rollup_sidecar_reproduces_a_rebuild_and_is_ignored_when_it_cannot_be_trusted() {
    let dir = test_dir("rollup-sidecar");
    let truth = {
        let store = seeded_store(&dir);
        seed(&store);
        fingerprint(&store)
    };

    // Sealing writes the sidecar, so no query ever has to pay for the decode.
    let sidecars = rollup_files(&dir);
    assert!(
        !sidecars.is_empty(),
        "sealing a segment must write its rollup sidecar"
    );

    // Reopened cold, the answer comes from the sidecar and is identical.
    {
        let store = seeded_store(&dir);
        assert_eq!(fingerprint(&store), truth, "reopened from the sidecar");
    }

    // A sidecar with a flipped byte fails its checksum, so it is rebuilt.
    {
        let mut bytes = std::fs::read(&sidecars[0]).expect("read sidecar");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&sidecars[0], &bytes).expect("write sidecar");
        let store = seeded_store(&dir);
        assert_eq!(
            fingerprint(&store),
            truth,
            "corrupt sidecar must be ignored"
        );
    }

    // A truncated sidecar — the shape a crash mid-write would leave if the
    // rename were not atomic — is rebuilt rather than half-read.
    {
        let bytes = std::fs::read(&sidecars[0]).expect("read sidecar");
        std::fs::write(&sidecars[0], &bytes[..bytes.len() / 3]).expect("truncate sidecar");
        let store = seeded_store(&dir);
        assert_eq!(fingerprint(&store), truth, "truncated sidecar is ignored");
    }

    // An EMPTY but well-formed-looking file, and a file of the right length
    // full of zeroes: neither may be believed.
    for filler in [Vec::new(), vec![0_u8; 512]] {
        std::fs::write(&sidecars[0], &filler).expect("write sidecar");
        let store = seeded_store(&dir);
        assert_eq!(fingerprint(&store), truth, "garbage sidecar is ignored");
    }

    // Deleting it entirely: the answer is unchanged AND the sidecar comes
    // back, so a store that predates this file heals on its first query.
    {
        std::fs::remove_file(&sidecars[0]).expect("remove sidecar");
        let store = seeded_store(&dir);
        assert_eq!(fingerprint(&store), truth, "absent sidecar is rebuilt");
        drop(store);
        assert!(
            sidecars[0].exists(),
            "a rebuilt rollup must be persisted, or every restart pays again"
        );
    }
}

/// A sidecar that describes a DIFFERENT segment must never be believed, even
/// though it is a perfectly valid, checksum-clean rollup file.
///
/// This is the failure the checksum cannot catch: the bytes are intact, they
/// are just about something else. Only the binding to the segment's identity
/// rules it out, and if it did not, the aggregates would be confidently and
/// silently wrong.
#[test]
fn a_rollup_sidecar_bound_to_another_segment_is_rejected() {
    let first = test_dir("rollup-binding-a");
    let second = test_dir("rollup-binding-b");

    let truth = {
        let store = seeded_store(&first);
        seed(&store);
        fingerprint(&store)
    };
    {
        // A different corpus, so its rollup is valid but describes nothing
        // in the first store.
        let store = seeded_store(&second);
        for index in 0..30_u64 {
            store
                .ingest(llm_span(
                    &format!("o-{index}"),
                    &format!("o-{index}"),
                    "other-session",
                    "some-other-model",
                    500_000 + index,
                    999,
                    999,
                    9.99,
                ))
                .expect("ingests");
        }
        store.flush().expect("seals");
    }

    let target = rollup_files(&first).remove(0);
    let donor = rollup_files(&second).remove(0);
    std::fs::copy(&donor, &target).expect("plant the foreign sidecar");

    let store = seeded_store(&first);
    assert_eq!(
        fingerprint(&store),
        truth,
        "a valid sidecar for another segment must be rejected, not trusted"
    );
}

/// Sidecars are removed with their segments, and any that a crash stranded
/// are swept at open.
#[test]
fn rollup_sidecars_do_not_outlive_their_segments() {
    let dir = test_dir("rollup-orphans");
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(1),
            ..Config::default()
        },
    )
    .expect("opens");
    store
        .ingest(llm_span("t", "s", "sess", "m", 1_000, 10, 10, 0.01))
        .expect("ingests");
    store.flush().expect("seals");
    store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(rollup_files(&dir).len(), 1);

    // Expiry drops the segment; its sidecar goes with it.
    store.compact_expired().expect("compacts");
    assert!(
        rollup_files(&dir).is_empty(),
        "a removed segment must take its sidecar with it: {:?}",
        rollup_files(&dir)
    );

    // A sidecar stranded by a crash between the two unlinks is swept at open.
    let stranded = dir.join("segment-00000000000000009999.rollup");
    std::fs::write(&stranded, b"stranded").expect("write");
    drop(store);
    let store = seeded_store(&dir);
    assert!(
        !stranded.exists(),
        "opening the store must sweep sidecars with no segment"
    );
    drop(store);
}

/// PARTIAL expiry rewrites a segment in place, and the aggregates must follow
/// it — in this process AND in the next one.
///
/// This is the one path where a segment's bytes change under a path that stays
/// the same, so it is the only place a cached rollup can outlive the data it
/// describes. Both caches are on the line: the in-memory entry, which
/// `expire_before` replaces under the same lock that swaps the segment, and
/// the on-disk sidecar, which the rewrite writes fresh. A miss in either one
/// reports spans that TTL deleted, and the reopen half is what catches a stale
/// sidecar — deletion a restart undoes is not deletion.
#[test]
fn partial_expiry_is_reflected_by_the_aggregates_and_survives_reopen() {
    let dir = test_dir("partial-expiry");
    // Expiry works on wall-clock `end_time_ns`, so the corpus is anchored to
    // now: half of it comfortably older than the cutoff, half comfortably
    // newer, with nothing near the boundary for a slow machine to reclassify.
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    let hour = 3_600_000_000_000_u64;
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            ..Config::default()
        },
    )
    .expect("opens");

    for index in 0..40_u64 {
        // Old: two hours back, so an hour's TTL covers it.
        store
            .ingest(llm_span(
                &format!("old-{index}"),
                &format!("old-{index}"),
                "sess-old",
                "expired-model",
                now_ns - 2 * hour,
                100,
                100,
                1.0,
            ))
            .expect("ingests");
        // New: right now.
        store
            .ingest(llm_span(
                &format!("new-{index}"),
                &format!("new-{index}"),
                "sess-live",
                "live-model",
                now_ns,
                7,
                3,
                0.25,
            ))
            .expect("ingests");
    }
    store.flush().expect("seals");

    // Warm the caches, so expiry has something stale to get wrong.
    let before = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(before.len(), 2, "both models present before expiry");

    let removed = store.compact_expired().expect("expires");
    assert_eq!(removed, 40, "exactly the old half expires");

    let live = |store: &Store| -> Vec<(String, usize, f64)> {
        let mut rows: Vec<(String, usize, f64)> = store
            .llm_aggregate(LlmGroupBy::Model, None, None)
            .expect("aggregates")
            .iter()
            .map(|row| (row.key.clone(), row.spans, row.cost_usd))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    };
    let expected = [("live-model".to_owned(), 40, 10.0)];

    let after = live(&store);
    assert_eq!(after.len(), 1, "the expired model is gone: {after:?}");
    assert_eq!(after[0].0, expected[0].0);
    assert_eq!(after[0].1, expected[0].1);
    assert!(
        (after[0].2 - expected[0].2).abs() < 1e-9,
        "cost after expiry was {}",
        after[0].2
    );

    // Sessions come off the same rollups by a different projection, so they
    // are a second, independent read of whether the cache followed the data.
    let sessions = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("sessions");
    assert_eq!(sessions.len(), 1, "only the live session remains");
    assert_eq!(sessions[0].session_id, "sess-live");
    assert_eq!(sessions[0].span_count, 40);

    // And the on-disk sidecar agrees: a fresh process must not read back the
    // pre-expiry counters.
    drop(store);
    let reopened = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            ..Config::default()
        },
    )
    .expect("reopens");
    let reloaded = live(&reopened);
    assert_eq!(
        reloaded.len(),
        1,
        "a reopened store must not resurrect expired spans: {reloaded:?}"
    );
    assert_eq!(reloaded[0].1, 40);
    assert!((reloaded[0].2 - 10.0).abs() < 1e-9);
}

/// TTL expiry must not re-read the corpus to discover that nothing aged out.
///
/// The sweep runs once a minute over every segment. It used to JSON-decode all
/// of them every time, which on a large store is the most expensive thing the
/// process does on a timer — and it is pure waste, because a segment whose
/// spans all end after the cutoff cannot contribute anything. The rollup's
/// end-time range answers that without a decode, and the counters are the only
/// way to tell: expiry removes exactly the same spans either way, so a correct
/// result says nothing about what it cost.
#[test]
fn expiry_rules_segments_out_without_decoding_them() {
    let dir = test_dir("expiry-skip");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    let hour = 3_600_000_000_000_u64;
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            ..Config::default()
        },
    )
    .expect("opens");

    // Three segments, each sealed separately and each entirely on one side of
    // the one-hour cutoff: two wholly expired, three wholly live.
    let seal = |label: &str, start_ns: u64, count: u64| {
        for index in 0..count {
            store
                .ingest(llm_span(
                    &format!("{label}-{index}"),
                    &format!("{label}-{index}"),
                    &format!("sess-{label}"),
                    "m",
                    start_ns,
                    10,
                    10,
                    0.5,
                ))
                .expect("ingests");
        }
        store.flush().expect("seals");
    };
    seal("old-a", now_ns - 5 * hour, 10);
    seal("old-b", now_ns - 4 * hour, 10);
    seal("live-a", now_ns, 10);
    seal("live-b", now_ns, 10);
    seal("live-c", now_ns, 10);

    let removed = store.compact_expired().expect("expires");
    assert_eq!(removed, 20, "both old segments' spans go");

    let metrics = store.metrics();
    let skipped = metrics.expiry_segments_skipped.get();
    let decoded = metrics.expiry_segments_decoded.get();
    assert_eq!(
        skipped + decoded,
        5,
        "every segment is accounted for exactly once"
    );
    assert_eq!(
        decoded, 0,
        "no segment straddles the cutoff, so none had to be read: \
         {skipped} skipped, {decoded} decoded"
    );

    // And the answer is still exactly right.
    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].spans, 30, "the three live segments survive whole");
}

/// A segment straddling the cutoff still gets read — the fast path must not
/// become a wrong answer for the one case it cannot rule out.
#[test]
fn expiry_still_reads_a_segment_that_straddles_the_cutoff() {
    let dir = test_dir("expiry-straddle");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    let hour = 3_600_000_000_000_u64;
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            ..Config::default()
        },
    )
    .expect("opens");

    // One segment, half either side of the cutoff.
    for index in 0..10_u64 {
        store
            .ingest(llm_span(
                &format!("old-{index}"),
                &format!("old-{index}"),
                "sess",
                "m",
                now_ns - 5 * hour,
                10,
                10,
                1.0,
            ))
            .expect("ingests");
        store
            .ingest(llm_span(
                &format!("new-{index}"),
                &format!("new-{index}"),
                "sess",
                "m",
                now_ns,
                1,
                1,
                0.5,
            ))
            .expect("ingests");
    }
    store.flush().expect("seals");

    assert_eq!(store.compact_expired().expect("expires"), 10);
    let metrics = store.metrics();
    assert_eq!(
        metrics.expiry_segments_decoded.get(),
        1,
        "a straddling segment must be read, not guessed at"
    );
    assert_eq!(metrics.expiry_segments_skipped.get(), 0);

    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(rows[0].spans, 10, "only the live half remains");
    assert!((rows[0].cost_usd - 5.0).abs() < 1e-9);
}

/// A store with no payloads must not even COMPUTE the live reference set.
///
/// `sweep_expired` returns immediately when there is no `payloads/` directory,
/// so the walk that produced its argument was pure waste — and that walk is
/// over every segment in the corpus.
#[test]
fn the_ttl_sweep_does_not_fill_the_rollup_cache() {
    let dir = test_dir("sweep-cache");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            ..Config::default()
        },
    )
    .expect("opens");
    for index in 0..30_u64 {
        store
            .ingest(llm_span(
                &format!("t-{index}"),
                &format!("s-{index}"),
                "sess",
                "m",
                now_ns,
                10,
                10,
                0.1,
            ))
            .expect("ingests");
        if index % 10 == 9 {
            store.flush().expect("seals");
        }
    }
    assert_eq!(
        store.cached_rollup_count().expect("count"),
        0,
        "sealing alone must not populate the analytics cache"
    );

    store.compact_expired().expect("sweeps");
    assert_eq!(
        store.cached_rollup_count().expect("count"),
        0,
        "a TTL tick must not pull the corpus into the rollup cache"
    );

    // A real aggregation still populates it — the cache is not broken, it is
    // just no longer filled by a timer.
    store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert!(
        store.cached_rollup_count().expect("count") > 0,
        "a query is what warms the cache"
    );
}

/// The same guarantee for a store that DOES have payloads, where the sweep
/// really runs.
///
/// The test above proves only that a store without a `payloads/` directory
/// skips the whole thing — it never reaches `live_payload_refs` at all, so it
/// cannot say anything about how that function reads rollups. This one
/// configures a payload threshold low enough that ordinary spans offload,
/// which makes the sweep genuinely execute, and then asserts the same
/// property: a timer must not pull the corpus into the cache.
#[test]
fn the_payload_sweep_reads_rollups_without_caching_them() {
    let dir = test_dir("sweep-payloads");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    let store = Store::open(
        &dir,
        Config {
            flush_spans: 100_000,
            ttl_seconds: Some(3_600),
            // Low enough that the prompt attribute below is offloaded.
            payload_threshold: Some(64),
            ..Config::default()
        },
    )
    .expect("opens");

    for index in 0..30_u64 {
        let mut span = llm_span(
            &format!("t-{index}"),
            &format!("s-{index}"),
            "sess",
            "m",
            now_ns,
            10,
            10,
            0.1,
        );
        span.attributes
            // Distinct per span: payload storage is content-addressed, so
            // thirty identical values dedupe to a single file and the sweep
            // would have one reference to get wrong instead of thirty.
            .insert(
                "gen_ai.prompt".to_owned(),
                json!(format!("prompt-{index}-{}", "x".repeat(512))),
            );
        store.ingest(span).expect("ingests");
        if index % 10 == 9 {
            store.flush().expect("seals");
        }
    }

    // The sweep only runs if there is something to sweep.
    assert!(
        dir.join("payloads").exists(),
        "the corpus must actually offload payloads for this test to mean anything"
    );
    let before = payload_files(&dir);
    assert!(before >= 30, "expected one payload per span, got {before}");
    assert_eq!(store.cached_rollup_count().expect("count"), 0);

    store.compact_expired().expect("sweeps");
    assert_eq!(
        store.cached_rollup_count().expect("count"),
        0,
        "the payload sweep must read sidecars without caching them"
    );

    // And nothing was wrongly swept. This is the half that matters most:
    // reading the reference set from a sidecar instead of from a freshly
    // built rollup must not UNDER-report, because a missed reference is a
    // payload file deleted while a live span still points at it.
    assert_eq!(
        payload_files(&dir),
        before,
        "every payload a live span references must survive the sweep"
    );
    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregates");
    assert_eq!(rows[0].spans, 30, "nothing expired, so nothing may vanish");
}

/// Every file under `payloads/`, recursively — the sweep's blast radius.
fn payload_files(dir: &std::path::Path) -> usize {
    fn walk(path: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| {
                let child = entry.path();
                if child.is_dir() {
                    walk(&child)
                } else {
                    1
                }
            })
            .sum()
    }
    walk(&dir.join("payloads"))
}
