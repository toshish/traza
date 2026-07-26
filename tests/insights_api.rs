//! The query surface the dashboard was rebuilt on: span status filtering,
//! cursor pagination with per-query cost, the aggregation routes, and
//! cross-trace annotation search.
//!
//! Each of these existed as a capability the engine already had and the HTTP
//! API could not express — "show me the failures" was unanswerable even though
//! every aggregate in the store counted errors — so the tests assert the
//! contract a client can actually rely on rather than the internals.

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
    let dir = std::env::temp_dir().join(format!(
        "traza-insights-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

const BASE_NS: u64 = 1_700_000_000_000_000_000;

/// A corpus with a known shape: 20 spans, every fourth one an error, durations
/// climbing so percentiles are predictable.
fn corpus() -> Value {
    let spans: Vec<Value> = (0..20u64)
        .map(|index| {
            let start = BASE_NS + index * 1_000_000_000;
            json!({
                "trace_id": format!("trace-{index:02}"),
                "span_id": "s1",
                "name": if index % 4 == 0 { "tool.lookup" } else { "llm.completion" },
                "service": if index % 2 == 0 { "svc-a" } else { "svc-b" },
                "status": if index % 4 == 0 { "error" } else { "ok" },
                "start_time_ns": start,
                "end_time_ns": start + (index + 1) * 10_000_000,
                "attributes": {
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.usage.prompt_tokens": 100,
                    "gen_ai.usage.completion_tokens": 10,
                    "llm.cost_usd": 0.001,
                },
            })
        })
        .collect();
    json!({ "spans": spans })
}

fn seed(server: &Server) {
    let (status, body) = server.request("POST", "/v1/spans", Some(&corpus()));
    assert_eq!(status, 200, "seed failed: {body}");
}

#[test]
fn status_filters_reach_the_span_field_not_an_attribute() {
    let dir = test_dir("status");
    let server = Server::spawn(&dir);
    seed(&server);

    let (status, body) = server.request("GET", "/v1/spans?status=error&limit=100", None);
    assert_eq!(status, 200, "{body}");
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 5, "every fourth of twenty is an error: {body}");
    assert!(spans.iter().all(|span| span["status"] == "error"));

    // The complement must be exact: `not_status` is the half of the filter
    // that makes "everything that did not fail" expressible.
    let (status, body) = server.request("GET", "/v1/spans?not_status=error&limit=100", None);
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 15);
    assert!(spans.iter().all(|span| span["status"] != "error"));

    // attr.status is a DIFFERENT filter — an attribute nobody wrote — and must
    // not silently answer as though it were the span's status.
    let (status, body) = server.request("GET", "/v1/spans?attr.status=error&limit=100", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["spans"].as_array().map(Vec::len),
        Some(0),
        "attr.status must not alias the span status field: {body}"
    );
    server.kill();
}

#[test]
fn a_cursor_pages_without_gaps_or_repeats() {
    let dir = test_dir("cursor");
    let server = Server::spawn(&dir);
    seed(&server);

    let (_, all) = server.request("GET", "/v1/spans?limit=100", None);
    let expected: Vec<String> = all["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .map(|span| span["trace_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(expected.len(), 20);

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let target = match &cursor {
            Some(token) => format!("/v1/spans?limit=6&cursor={token}"),
            None => "/v1/spans?limit=6".to_owned(),
        };
        let (status, page) = server.request("GET", &target, None);
        assert_eq!(status, 200, "{page}");
        let spans = page["spans"].as_array().expect("spans");
        for span in spans {
            seen.push(span["trace_id"].as_str().unwrap_or_default().to_owned());
        }
        match page["next_cursor"].as_str() {
            Some(token) => cursor = Some(token.to_owned()),
            None => break,
        }
    }
    assert_eq!(
        seen, expected,
        "paging must reproduce the unpaged order exactly"
    );

    // A short page has already reached the end; offering a cursor there would
    // invite a round trip that can only return nothing.
    let (_, page) = server.request("GET", "/v1/spans?limit=100", None);
    assert!(page["next_cursor"].is_null(), "{page}");

    let (status, body) = server.request("GET", "/v1/spans?cursor=not-a-real-token", None);
    assert_eq!(status, 400, "a hand-edited cursor must fail loudly: {body}");
    server.kill();
}

#[test]
fn a_query_reports_what_it_cost() {
    let dir = test_dir("cost");
    let server = Server::spawn(&dir);
    seed(&server);
    server.request("POST", "/v1/flush", None);

    let (status, body) = server.request("GET", "/v1/spans?limit=5", None);
    assert_eq!(status, 200, "{body}");
    let cost = &body["cost"];
    assert!(
        cost["elapsed_ns"].as_u64().is_some(),
        "elapsed missing: {body}"
    );
    assert!(
        cost["segments_examined"].as_u64().is_some(),
        "examined missing: {body}"
    );
    assert!(
        cost["segments_pruned"].as_u64() <= cost["segments_examined"].as_u64(),
        "cannot prune more than examined: {body}"
    );

    // A window before the corpus must prune every segment it examines — that
    // is the whole claim a time filter makes.
    let (_, body) = server.request(
        "GET",
        &format!("/v1/spans?until={}&limit=5", BASE_NS - 1_000_000_000),
        None,
    );
    let cost = &body["cost"];
    assert_eq!(
        cost["segments_pruned"], cost["segments_examined"],
        "an out-of-range window must prune everything it looked at: {body}"
    );
    server.kill();
}

#[test]
fn a_series_buckets_the_window_without_losing_spans() {
    let dir = test_dir("series");
    let server = Server::spawn(&dir);
    seed(&server);

    let until = BASE_NS + 21_000_000_000;
    let (status, body) = server.request(
        "GET",
        &format!("/v1/stats/series?since={BASE_NS}&until={until}&buckets=5"),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let buckets = body["buckets"].as_array().expect("buckets");
    assert_eq!(buckets.len(), 5, "exactly the requested count: {body}");

    let total: u64 = buckets
        .iter()
        .map(|bucket| bucket["spans"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(
        total, 20,
        "every in-window span lands in exactly one bucket"
    );

    let errors: u64 = buckets
        .iter()
        .map(|bucket| bucket["errors"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(errors, 5);

    // Percentiles are per bucket, not a mean over the window.
    assert!(buckets
        .iter()
        .any(|bucket| bucket["p95_ns"].as_u64().unwrap_or(0) > 0));

    let (status, body) = server.request("GET", "/v1/stats/series?buckets=5", None);
    assert_eq!(status, 400, "a series needs a window: {body}");
    server.kill();
}

#[test]
fn a_duration_histogram_bounds_its_percentiles() {
    let dir = test_dir("duration");
    let server = Server::spawn(&dir);
    seed(&server);

    let (status, body) = server.request("GET", "/v1/stats/duration", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 20);

    let p50 = body["p50_ns"].as_u64().expect("p50");
    let p95 = body["p95_ns"].as_u64().expect("p95");
    let p99 = body["p99_ns"].as_u64().expect("p99");
    let max = body["max_ns"].as_u64().expect("max");
    assert!(
        p50 <= p95 && p95 <= p99 && p99 <= max,
        "percentiles must ascend: {body}"
    );

    // Durations are 10ms..200ms; the true p95 is the 19th of 20 = 190ms, and a
    // reported percentile must be at or above the truth but within 1/16 of it.
    let truth = 190_000_000f64;
    let error = (p95 as f64 - truth) / truth;
    assert!(
        (0.0..=0.0625).contains(&error),
        "p95 {p95} is {error} off a true {truth}: {body}"
    );

    // Only occupied buckets travel — the payload is the distribution, not a
    // thousand zeros.
    let buckets = body["buckets"].as_array().expect("buckets");
    assert!(!buckets.is_empty() && buckets.len() <= 20, "{body}");
    assert!(buckets
        .iter()
        .all(|bucket| bucket["count"].as_u64().unwrap_or(0) > 0));
    server.kill();
}

#[test]
fn failures_group_by_signature_with_a_way_in() {
    let dir = test_dir("failures");
    let server = Server::spawn(&dir);
    seed(&server);

    let (status, body) = server.request("GET", "/v1/stats/failures", None);
    assert_eq!(status, 200, "{body}");
    let groups = body["groups"].as_array().expect("groups");
    assert!(!groups.is_empty(), "five errors must group: {body}");

    let total: u64 = groups
        .iter()
        .map(|group| group["count"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(total, 5, "every error is in exactly one group: {body}");

    for group in groups {
        assert_eq!(group["status"], "error");
        assert!(
            group["first_seen_ns"].as_u64() <= group["last_seen_ns"].as_u64(),
            "first seen cannot follow last seen: {group}"
        );
        // The example is what turns a count into a place you can go.
        let example = group["example_trace_id"].as_str().unwrap_or_default();
        assert!(!example.is_empty(), "a group must open: {group}");
        let (status, trace) = server.request("GET", &format!("/v1/traces/{example}"), None);
        assert_eq!(status, 200, "the example must resolve: {trace}");
    }
    server.kill();
}

#[test]
fn slowest_ranks_the_tail_exactly() {
    let dir = test_dir("slowest");
    let server = Server::spawn(&dir);
    seed(&server);

    let (status, body) = server.request("GET", "/v1/stats/slowest?limit=3", None);
    assert_eq!(status, 200, "{body}");
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 3);

    let durations: Vec<u64> = spans
        .iter()
        .map(|span| {
            span["end_time_ns"].as_u64().unwrap_or(0) - span["start_time_ns"].as_u64().unwrap_or(0)
        })
        .collect();
    let mut sorted = durations.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(durations, sorted, "must come back slowest first: {body}");
    // The corpus tops out at 200ms; the slowest must actually be the slowest,
    // not the slowest of an arbitrary first page.
    assert_eq!(durations[0], 200_000_000, "{body}");
    server.kill();
}

#[test]
fn annotations_read_across_traces() {
    let dir = test_dir("annotations");
    let server = Server::spawn(&dir);
    seed(&server);

    for index in 0..6u64 {
        let (status, body) = server.request(
            "POST",
            "/v1/annotations",
            Some(&json!({
                "trace_id": format!("trace-{index:02}"),
                "name": if index % 2 == 0 { "groundedness" } else { "helpfulness" },
                "value": 0.5 + (index as f64) / 20.0,
                "source": if index % 3 == 0 { "human:reviewer" } else { "eval:nightly" },
                "timestamp_ns": BASE_NS + index * 1_000_000_000,
            })),
        );
        assert_eq!(status, 200, "{body}");
    }

    // The whole point: no trace_id. An eval run is a population, and requiring
    // a trace made it readable only one trace at a time.
    let (status, body) = server.request("GET", "/v1/annotations", None);
    assert_eq!(status, 200, "{body}");
    let all = body["annotations"].as_array().expect("annotations");
    assert_eq!(all.len(), 6, "{body}");

    // Newest first, so a review queue starts at what just landed.
    let stamps: Vec<u64> = all
        .iter()
        .map(|a| a["timestamp_ns"].as_u64().unwrap_or(0))
        .collect();
    let mut descending = stamps.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(stamps, descending, "{body}");

    let (_, body) = server.request("GET", "/v1/annotations?name=groundedness", None);
    let narrowed = body["annotations"].as_array().expect("annotations");
    assert_eq!(narrowed.len(), 3);
    assert!(narrowed.iter().all(|a| a["name"] == "groundedness"));

    // Source is a prefix match so `human:` and `eval:` separate a review queue
    // from a nightly run without an exact-string match on the whole value.
    let (_, body) = server.request("GET", "/v1/annotations?source=human:", None);
    let humans = body["annotations"].as_array().expect("annotations");
    assert_eq!(humans.len(), 2);
    assert!(humans.iter().all(|a| a["source"]
        .as_str()
        .unwrap_or_default()
        .starts_with("human:")));

    let (_, body) = server.request("GET", "/v1/annotations?trace_id=trace-00", None);
    assert_eq!(body["annotations"].as_array().map(Vec::len), Some(1));

    let (_, body) = server.request("GET", "/v1/annotations?limit=2", None);
    assert_eq!(body["annotations"].as_array().map(Vec::len), Some(2));
    server.kill();
}

#[test]
fn metrics_json_reports_per_route_latency_with_its_error_bound() {
    let dir = test_dir("metrics");
    let server = Server::spawn(&dir);
    seed(&server);
    server.request("GET", "/v1/spans?limit=5", None);
    server.request("GET", "/v1/stats/duration", None);

    let (status, body) = server.request("GET", "/v1/metrics.json", None);
    assert_eq!(status, 200, "{body}");

    assert!(body["uptime_ns"].as_u64().is_some(), "{body}");
    assert!(
        body["requests"]["total"].as_u64().unwrap_or(0) > 0,
        "{body}"
    );

    // Route classes exist so "how fast are queries" has an answer: one blended
    // histogram over ingest and search described neither.
    for class in ["ingest", "lookup", "search", "stats", "other"] {
        assert!(
            body["by_class"][class].is_object(),
            "missing {class}: {body}"
        );
    }
    assert!(
        body["by_class"]["search"]["count"].as_u64().unwrap_or(0) > 0,
        "a span search must be counted as search: {body}"
    );
    assert!(
        body["by_class"]["ingest"]["count"].as_u64().unwrap_or(0) > 0,
        "the seed POST must be counted as ingest: {body}"
    );

    // The bound is published alongside the numbers rather than left implicit.
    assert_eq!(body["percentile_error_bound"], 0.0625, "{body}");

    let search = &body["by_class"]["search"];
    let p50 = search["p50_ns"].as_u64().expect("p50");
    let p95 = search["p95_ns"].as_u64().expect("p95");
    assert!(p50 <= p95, "percentiles must ascend: {body}");
    server.kill();
}

#[test]
fn prometheus_output_carries_the_new_series() {
    let dir = test_dir("prom");
    let server = Server::spawn(&dir);
    seed(&server);
    server.request("GET", "/v1/spans?limit=1", None);

    // /v1/metrics is text, not JSON, so read it as bytes through a raw request.
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    write!(
        stream,
        "GET /v1/metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");

    for metric in [
        "traza_uptime_seconds",
        "traza_http_search_ns_p95",
        "traza_http_request_ns_p95",
        "traza_http_responses_2xx_total",
        "traza_wal_fsync_ns_p95",
    ] {
        assert!(text.contains(metric), "missing {metric} in:\n{text}");
    }
    server.kill();
}
