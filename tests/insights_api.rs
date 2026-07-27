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

#[test]
fn content_search_composes_with_the_aggregations() {
    // The merge that brought content search together with these routes put a
    // filter the aggregations had never seen through `fold_spans`. Nothing
    // covered that pairing: each side was tested against the search path only,
    // and a fold that dropped `content` would answer these routes over the
    // whole corpus while looking perfectly healthy.
    let dir = test_dir("content-fold");
    let server = Server::spawn(&dir);

    let spans: Vec<Value> = (0..10u64)
        .map(|index| {
            let start = BASE_NS + index * 1_000_000_000;
            json!({
                "trace_id": format!("ct-{index:02}"),
                "span_id": "s1",
                "name": "tool.lookup",
                "service": "svc",
                "status": if index < 3 { "error" } else { "ok" },
                "start_time_ns": start,
                "end_time_ns": start + (index + 1) * 10_000_000,
                // Half the corpus is about refunds, half about shipping. The
                // words are distinct so a leaked filter is unmistakable.
                "attributes": { "note": if index < 5 { "refund the order" } else { "shipping label" } },
            })
        })
        .collect();
    let (status, body) = server.request("POST", "/v1/spans", Some(&json!({ "spans": spans })));
    assert_eq!(status, 200, "{body}");

    // Baseline: the search path agrees the split is five and five.
    let (_, body) = server.request("GET", "/v1/spans?content=refund&limit=100", None);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(5), "{body}");

    // The distribution must see the same five, not all ten.
    let (status, body) = server.request("GET", "/v1/stats/duration?content=refund", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 5, "content must narrow the fold: {body}");

    // Failures: three of the refund spans are errors, none of the shipping ones.
    let (status, body) = server.request("GET", "/v1/stats/failures?content=refund", None);
    assert_eq!(status, 200, "{body}");
    let total: u64 = body["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|group| group["count"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(total, 3, "{body}");

    // A word in the corpus but not in this half must fold to nothing.
    let (_, body) = server.request("GET", "/v1/stats/duration?content=shipping", None);
    assert_eq!(body["count"], 5, "{body}");
    let (_, body) = server.request("GET", "/v1/stats/duration?content=refund%20shipping", None);
    assert_eq!(
        body["count"], 0,
        "words are ANDed, so no span has both: {body}"
    );

    // The series buckets the narrowed set, not the corpus.
    let until = BASE_NS + 11_000_000_000;
    let (status, body) = server.request(
        "GET",
        &format!("/v1/stats/series?since={BASE_NS}&until={until}&buckets=5&content=refund"),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let counted: u64 = body["buckets"]
        .as_array()
        .expect("buckets")
        .iter()
        .map(|bucket| bucket["spans"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(counted, 5, "{body}");

    // And the tail ranks within the narrowed set: the slowest refund span is
    // index 4 at 50ms, not index 9 at 100ms.
    let (_, body) = server.request("GET", "/v1/stats/slowest?content=refund&limit=1", None);
    let span = &body["spans"][0];
    let duration =
        span["end_time_ns"].as_u64().unwrap_or(0) - span["start_time_ns"].as_u64().unwrap_or(0);
    assert_eq!(duration, 50_000_000, "ranked outside the filter: {body}");

    server.kill();
}

#[test]
fn extreme_but_valid_inputs_answer_instead_of_panicking() {
    // Every one of these was a panic reachable from a well-formed request, so
    // the server closed the connection with no response at all. They are
    // arithmetic edges, not malformed input: a span really can carry
    // `end_time_ns = u64::MAX`, and a caller really can ask for an unbounded
    // window.
    let dir = test_dir("extremes");
    let server = Server::spawn(&dir);
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!({"spans": [{
            "trace_id": "t", "span_id": "s", "name": "n", "service": "v",
            "status": "error", "start_time_ns": 0u64, "end_time_ns": u64::MAX,
            "attributes": {},
        }]})),
    );
    assert_eq!(status, 200, "{body}");

    // The top duration bucket's upper bound is not representable.
    let (status, body) = server.request("GET", "/v1/stats/duration", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 1);
    assert!(body["p95_ns"].as_u64().is_some(), "{body}");

    // `span + buckets - 1` overflows on an unbounded window.
    let (status, body) = server.request(
        "GET",
        &format!("/v1/stats/series?since=0&until={}&buckets=4", u64::MAX),
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["buckets"].as_array().map(Vec::len), Some(4), "{body}");

    // `Vec::with_capacity(limit + 1)` overflows, and an unbounded limit is a
    // request to hold the match set in memory.
    let (status, body) = server.request(
        "GET",
        &format!("/v1/stats/slowest?limit={}", u64::MAX),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = server.request(
        "GET",
        &format!("/v1/stats/failures?limit={}", u64::MAX),
        None,
    );
    assert_eq!(status, 200, "{body}");

    // A reversed window is the caller's error, and says so rather than
    // producing a bucket count nobody asked for.
    let (status, _) = server.request(
        "GET",
        &format!("/v1/stats/series?since={}&until=0&buckets=4", u64::MAX),
        None,
    );
    assert_eq!(status, 400);

    // Zero buckets is clamped, not divided by.
    let (status, body) = server.request("GET", "/v1/stats/series?since=0&until=10&buckets=0", None);
    assert_eq!(status, 200, "{body}");
    server.kill();
}

#[test]
fn a_failure_report_states_its_own_totals_and_truncation() {
    // The share of "all failures" a signature accounts for was computed in the
    // browser by summing the groups it had been sent — a page truncated to a
    // limit — so every share was inflated by exactly the amount the response
    // left out. The denominator now comes from the server.
    let dir = test_dir("failure-total");
    let server = Server::spawn(&dir);

    // Twelve distinct signatures, one span each, plus nine more on one of them.
    let mut spans: Vec<Value> = Vec::new();
    for index in 0..12u64 {
        spans.push(json!({
            "trace_id": format!("f-{index:02}"), "span_id": "s", "name": format!("op{index}"),
            "service": "svc", "status": "error",
            "start_time_ns": BASE_NS + index, "end_time_ns": BASE_NS + index + 1_000_000,
            "attributes": {},
        }));
    }
    for extra in 0..9u64 {
        spans.push(json!({
            "trace_id": format!("f-hot-{extra}"), "span_id": "s", "name": "op0",
            "service": "svc", "status": "error",
            "start_time_ns": BASE_NS + 100 + extra, "end_time_ns": BASE_NS + 100 + extra + 1_000_000,
            "attributes": {},
        }));
    }
    let (status, body) = server.request("POST", "/v1/spans", Some(&json!({"spans": spans})));
    assert_eq!(status, 200, "{body}");

    // Ask for three groups out of twelve.
    let (status, body) = server.request("GET", "/v1/stats/failures?limit=3", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["groups"].as_array().map(Vec::len), Some(3), "{body}");
    assert_eq!(body["total"], 21, "total counts every failed span: {body}");
    assert_eq!(body["distinct"], 12, "{body}");
    assert_eq!(body["groups_omitted"], 9, "{body}");
    assert_eq!(body["spans_untracked"], 0, "{body}");

    // The top signature is 10 of 21 — under half. Summing the returned page
    // (10 + 1 + 1 = 12) would have reported it as 83%.
    let top = body["groups"][0]["count"].as_u64().expect("count");
    assert_eq!(top, 10, "{body}");
    let honest = (top as f64) / body["total"].as_f64().expect("total");
    assert!(
        honest < 0.5,
        "share must be against the real total: {honest}"
    );
    server.kill();
}

#[test]
fn a_cursor_returns_spans_that_share_one_timestamp() {
    // The live tail advanced its watermark to `max(start_time_ns) + 1`, so when
    // more spans shared the last timestamp of a page than the page could hold,
    // the remainder were skipped forever. An SDK flushing a batch produces
    // exactly that: many spans, one timestamp. Paging by cursor has to reach
    // all of them, and this is the property the tail now depends on.
    let dir = test_dir("same-timestamp");
    let server = Server::spawn(&dir);

    // 250 spans, every one at the SAME start time, so nothing but the full
    // ordering key can separate them.
    let spans: Vec<Value> = (0..250u64)
        .map(|index| {
            json!({
                "trace_id": format!("same-{index:04}"),
                "span_id": "s1",
                "name": "batch.flush",
                "service": "svc",
                "status": "ok",
                "start_time_ns": BASE_NS,
                "end_time_ns": BASE_NS + 1_000_000,
                "attributes": {},
            })
        })
        .collect();
    let (status, body) = server.request("POST", "/v1/spans", Some(&json!({"spans": spans})));
    assert_eq!(status, 200, "{body}");

    // Page through in tens. Every span must appear exactly once.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..40 {
        let target = match &cursor {
            Some(token) => format!("/v1/spans?limit=10&cursor={token}"),
            None => "/v1/spans?limit=10".to_owned(),
        };
        let (status, page) = server.request("GET", &target, None);
        assert_eq!(status, 200, "{page}");
        for span in page["spans"].as_array().expect("spans") {
            seen.push(span["trace_id"].as_str().unwrap_or_default().to_owned());
        }
        match page["next_cursor"].as_str() {
            Some(token) => cursor = Some(token.to_owned()),
            None => break,
        }
    }

    assert_eq!(
        seen.len(),
        250,
        "paging must reach every span at the shared timestamp"
    );
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 250, "and must not repeat any of them");

    // The watermark approach the tail used to take: everything at this
    // timestamp is >= it, so `since` alone can never advance past the batch.
    // Confirm the API does return them all under a plain `since`, which is
    // what makes the cursor the only correct way to drain.
    let (_, body) = server.request("GET", &format!("/v1/spans?since={BASE_NS}&limit=10"), None);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(10));
    assert!(
        body["next_cursor"].as_str().is_some(),
        "a full page inside one timestamp must offer a cursor: {body}"
    );
    server.kill();
}

#[test]
fn a_series_window_at_the_top_of_the_range_answers_and_covers_itself() {
    // The previous round fixed the bucket WIDTH and left the bucket STARTS
    // unchecked three lines below it, so a window near `u64::MAX` still
    // panicked — `since + width * index` overflows long before the last
    // bucket. The saturating ceiling was also one nanosecond short per bucket
    // on a full-range window, which left the last bucket ending before the
    // window did.
    let dir = test_dir("series-top");
    let server = Server::spawn(&dir);

    for window in [
        format!("since={}&until={}", u64::MAX - 10, u64::MAX),
        format!("since=0&until={}", u64::MAX),
        format!("since={}&until={}", u64::MAX - 1, u64::MAX),
    ] {
        let target = format!("/v1/stats/series?{window}&buckets=24");
        let (status, body) = server.request("GET", &target, None);
        assert_eq!(status, 200, "{target} -> {body}");

        let buckets = body["buckets"].as_array().expect("buckets");
        assert_eq!(buckets.len(), 24, "{target}");

        let width = body["bucket_ns"].as_u64().expect("bucket_ns");
        let until = body["until_ns"].as_u64().expect("until_ns");
        let since = body["since_ns"].as_u64().expect("since_ns");

        // Ascending, never past the window, and the last one reaches its end.
        let mut previous = 0u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let start = bucket["start_ns"].as_u64().expect("start_ns");
            assert!(
                start >= since,
                "bucket {index} starts before the window: {body}"
            );
            assert!(
                start <= until,
                "bucket {index} starts past the window: {body}"
            );
            if index > 0 {
                assert!(start >= previous, "bucket starts went backwards: {body}");
            }
            previous = start;
        }
        let last = buckets[buckets.len() - 1]["start_ns"]
            .as_u64()
            .expect("start_ns");
        assert!(
            last.saturating_add(width) >= until,
            "the last bucket ends before the window does — the ceiling is short: {body}"
        );
    }
    server.kill();
}

#[test]
fn a_series_covers_its_window_for_ordinary_sizes_too() {
    // The coverage property is not special to the extremes; a window that does
    // not divide evenly by the bucket count must still be covered.
    let dir = test_dir("series-cover");
    let server = Server::spawn(&dir);
    for (span, buckets) in [
        (100u64, 24usize),
        (1_000, 7),
        (23, 24),
        (1, 512),
        (1_000_000_007, 48),
    ] {
        let target = format!(
            "/v1/stats/series?since={BASE_NS}&until={}&buckets={buckets}",
            BASE_NS + span
        );
        let (status, body) = server.request("GET", &target, None);
        assert_eq!(status, 200, "{target} -> {body}");
        let rows = body["buckets"].as_array().expect("buckets");
        assert_eq!(rows.len(), buckets, "{target}");
        let width = body["bucket_ns"].as_u64().expect("bucket_ns");
        let last = rows[rows.len() - 1]["start_ns"].as_u64().expect("start_ns");
        assert!(
            last + width >= BASE_NS + span,
            "span {span} over {buckets} buckets leaves the tail uncovered: {body}"
        );
    }
    server.kill();
}

#[test]
fn a_period_percentile_is_not_the_worst_buckets_percentile() {
    // The Overview screen derived a period p95 as `max(bucket.p95)` over the
    // series. That is not a percentile of the period, and this corpus shows
    // how far apart the two can be: one sparse bucket of slow spans sits
    // beside many buckets of fast ones, so the worst bucket's p95 is seconds
    // while the period's p95 is milliseconds.
    let dir = test_dir("period-p95");
    let server = Server::spawn(&dir);

    let mut spans: Vec<Value> = Vec::new();
    // 23 buckets' worth of fast traffic: 100 spans each at 1ms.
    for bucket in 0..23u64 {
        for index in 0..100u64 {
            let start = BASE_NS + bucket * 1_000_000_000 + index;
            spans.push(json!({
                "trace_id": format!("fast-{bucket}-{index}"), "span_id": "s",
                "name": "op", "service": "svc", "status": "ok",
                "start_time_ns": start, "end_time_ns": start + 1_000_000,
                "attributes": {},
            }));
        }
    }
    // One sparse bucket: 3 spans at 10 seconds each.
    for index in 0..3u64 {
        let start = BASE_NS + 23 * 1_000_000_000 + index;
        spans.push(json!({
            "trace_id": format!("slow-{index}"), "span_id": "s",
            "name": "op", "service": "svc", "status": "ok",
            "start_time_ns": start, "end_time_ns": start + 10_000_000_000u64,
            "attributes": {},
        }));
    }
    let (status, body) = server.request("POST", "/v1/spans", Some(&json!({"spans": spans})));
    assert_eq!(status, 200, "{body}");

    let until = BASE_NS + 24_000_000_000;
    let (_, series) = server.request(
        "GET",
        &format!("/v1/stats/series?since={BASE_NS}&until={until}&buckets=24"),
        None,
    );
    let worst_bucket_p95 = series["buckets"]
        .as_array()
        .expect("buckets")
        .iter()
        .map(|bucket| bucket["p95_ns"].as_u64().unwrap_or(0))
        .max()
        .unwrap_or(0);

    let (_, duration) = server.request(
        "GET",
        &format!("/v1/stats/duration?since={BASE_NS}&until={until}"),
        None,
    );
    let period_p95 = duration["p95_ns"].as_u64().expect("p95_ns");

    // The period's p95: 2,303 spans, 3 of them slow — the 95th percentile is
    // comfortably inside the fast population.
    assert!(
        period_p95 < 100_000_000,
        "the true period p95 should be milliseconds, got {period_p95}: {duration}"
    );
    // The worst bucket's p95 is the slow one, three orders of magnitude away.
    assert!(
        worst_bucket_p95 > 5_000_000_000,
        "the worst bucket should be seconds, got {worst_bucket_p95}: {series}"
    );
    assert!(
        worst_bucket_p95 > period_p95 * 50,
        "max-of-buckets ({worst_bucket_p95}) must be shown to diverge wildly \
         from the period p95 ({period_p95}) — that divergence is why the screen \
         reads the histogram instead"
    );
    server.kill();
}
