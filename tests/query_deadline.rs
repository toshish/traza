//! The per-request compute deadline: `--query-deadline-ms`.
//!
//! The budget's contract has three edges, and each gets a test: an
//! over-budget query is REFUSED — a 400 naming the budget, never a partial
//! answer dressed as a complete one — on every budgeted path, the trace
//! lookup included; the default budget never touches a legitimate query
//! (the worst measured legitimate query in this repo is ~3 s against a
//! 30 s default); and the documented exemptions really are exempt — an
//! export streams its whole dataset to the complete trailer under a 1 ms
//! budget.
//!
//! Every corpus here is seeded over HTTP, batched, so the spans travel the
//! same ingest path an operator's do, and every deadline server runs with a
//! 1 ms budget against a corpus whose scan reliably costs orders of
//! magnitude more.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, extra: &[&str]) -> Self {
        Self::spawn_with_env(data_dir, extra, &[])
    }

    fn spawn_with_env(data_dir: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("buffered")
            .env_remove("TRAZA_TOKENS")
            .env_remove("TRAZA_TEST_PANIC")
            // Explicit, so the socket timeout can never shadow the compute
            // deadline these tests are about.
            .env("TRAZA_SOCKET_TIMEOUT_MS", "30000")
            .stderr(Stdio::piped());
        for argument in extra {
            command.arg(argument);
        }
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = BufReader::new(stderr);
        let port = {
            let mut line = String::new();
            let mut startup = String::new();
            loop {
                line.clear();
                // A zero-length read is EOF: the server exited before it
                // listened. Looping on that spins forever and looks exactly
                // like a hung test, so report what it actually said instead.
                if reader.read_line(&mut line).expect("stderr read") == 0 {
                    panic!("traza-server exited before listening:\n{startup}");
                }
                startup.push_str(&line);
                if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                    break rest.trim().parse::<u16>().expect("port parses");
                }
            }
        };
        std::thread::spawn(move || for _ in reader.lines() {});
        Self { child, port }
    }

    fn connect(&self) -> TcpStream {
        let mut attempt = 0;
        loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .expect("timeout");
                    break stream;
                }
                Err(_) if attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect: {error}"),
            }
        }
    }
}

// A panicking test must never leak its server child: cargo waits on the
// child's inherited pipes, hanging the whole test binary long after the
// failure it should be reporting.
impl Drop for Server {
    fn drop(&mut self) {
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
        "traza-deadline-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// One request, one connection, the whole response: head and body split at
/// the blank line. `Connection: close` makes read-to-end the framing.
fn get(server: &Server, target: &str) -> (String, String) {
    let mut stream = server.connect();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    .expect("request writes");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("response reads");
    let text = String::from_utf8_lossy(&response).into_owned();
    match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head.to_owned(), body.to_owned()),
        None => (text, String::new()),
    }
}

/// One named counter out of the raw Prometheus text. Raw on purpose: the
/// JSON surface is a different rendering, and the scrape text is what an
/// operator's alerting reads.
fn metric(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .and_then(|value| value.trim().parse().ok())
}

/// Nanosecond origin every seeded span's timestamps count from.
const BASE_NS: u64 = 1_700_000_000_000_000_000;

/// Seeds `total` spans over HTTP in batches, all in one session, then
/// flushes so the corpus sits in segments rather than the write buffer.
fn seed(server: &Server, total: usize) {
    seed_with(server, total, &|index| {
        json!({
            "trace_id": format!("trace-{index}"),
            "span_id": format!("span-{index}"),
            "name": format!("checkout retry payment {index}"),
            "service": "svc",
            "start_time_ns": BASE_NS + (index as u64) * 1_000,
            "end_time_ns": BASE_NS + (index as u64) * 1_000 + 500,
            "attributes": {"session.id": "night-shift"},
        })
    });
}

/// The same corpus with every span in ONE trace, so a single
/// `GET /v1/traces/{id}` has to decode all of it.
fn seed_single_trace(server: &Server, total: usize) {
    seed_with(server, total, &|index| {
        json!({
            "trace_id": "mega",
            "span_id": format!("span-{index}"),
            "name": format!("checkout retry payment {index}"),
            "service": "svc",
            "start_time_ns": BASE_NS + (index as u64) * 1_000,
            "end_time_ns": BASE_NS + (index as u64) * 1_000 + 500,
            "attributes": {"session.id": "night-shift"},
        })
    });
}

fn seed_with(server: &Server, total: usize, make_span: &dyn Fn(usize) -> Value) {
    const BATCH: usize = 2_000;
    let mut written = 0;
    while written < total {
        let count = BATCH.min(total - written);
        let spans: Vec<Value> = (written..written + count).map(make_span).collect();
        let body = serde_json::to_string(&spans).expect("batch encodes");
        let mut stream = server.connect();
        write!(
            stream,
            "POST /v1/spans HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("head writes");
        stream.write_all(body.as_bytes()).expect("body writes");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response reads");
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "seed batch refused: {:.120}",
            text
        );
        written += count;
    }
    let (head, _) = {
        let mut stream = server.connect();
        write!(
            stream,
            "POST /v1/flush HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("flush writes");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("flush reads");
        let text = String::from_utf8_lossy(&response).into_owned();
        match text.split_once("\r\n\r\n") {
            Some((head, body)) => (head.to_owned(), body.to_owned()),
            None => (text, String::new()),
        }
    };
    assert!(head.starts_with("HTTP/1.1 200"), "flush failed: {head:.80}");
}

/// The corpus every deadline test scans: large enough that decoding it
/// reliably costs far more than the 1 ms budgets below, on any machine slow
/// enough to run tests at all.
const CORPUS: usize = 50_000;

#[test]
fn an_unindexed_content_search_past_its_budget_answers_400_and_is_counted() {
    let dir = test_dir("content-400");
    let server = Server::spawn(
        &dir,
        &[
            "--no-content-index",
            "--query-deadline-ms",
            "1",
            "--flush-spans",
            "5000",
        ],
    );
    seed(&server, CORPUS);

    // Without a content index nothing prunes, so this word forces a decode
    // of every record — and with a limit, the k-way merge spins past it
    // forever finding no match. That merge is exactly the loop the deadline
    // must reach.
    let asked = Instant::now();
    let (head, body) = get(&server, "/v1/spans?q=zzznotpresent&limit=10");
    // A tripwire, not the oracle: the 400 below is the proof, this only
    // catches a server that hung instead of refusing.
    assert!(
        asked.elapsed() < Duration::from_secs(20),
        "refusal took {:?}",
        asked.elapsed()
    );
    assert!(head.starts_with("HTTP/1.1 400"), "not a 400: {head:.80}");
    let payload: Value = serde_json::from_str(&body).expect("error body parses");
    let error = payload["error"].as_str().expect("error is a string");
    assert!(
        error.starts_with("query deadline exceeded: "),
        "wrong error: {error}"
    );

    let (_, metrics) = get(&server, "/v1/metrics");
    let counted = metric(&metrics, "traza_query_deadline_exceeded_total").unwrap_or(0);
    assert!(counted >= 1, "refusal was not counted:\n{metrics:.400}");
}

#[test]
fn the_same_search_under_the_default_budget_answers_200() {
    let dir = test_dir("default-200");
    // No --query-deadline-ms: the 30 s default. The worst legitimate query
    // in this repo measures ~3 s, so a corpus scan an order of magnitude
    // smaller must never brush the default.
    let server = Server::spawn(&dir, &["--no-content-index", "--flush-spans", "5000"]);
    seed(&server, CORPUS);
    let (head, _) = get(&server, "/v1/spans?q=zzznotpresent&limit=10");
    assert!(head.starts_with("HTTP/1.1 200"), "not a 200: {head:.80}");
}

#[test]
fn a_zero_budget_disables_the_deadline_entirely() {
    let dir = test_dir("zero-off");
    let server = Server::spawn(
        &dir,
        &[
            "--no-content-index",
            "--query-deadline-ms",
            "0",
            "--flush-spans",
            "5000",
        ],
    );
    seed(&server, CORPUS);
    let (head, _) = get(&server, "/v1/spans?q=zzznotpresent&limit=10");
    assert!(head.starts_with("HTTP/1.1 200"), "not a 200: {head:.80}");
}

#[test]
fn the_fold_path_and_the_session_path_enforce_the_same_budget() {
    let dir = test_dir("fold-session");
    let server = Server::spawn(&dir, &["--query-deadline-ms", "1", "--flush-spans", "5000"]);
    seed(&server, CORPUS);

    // The fold path: a whole-corpus histogram decodes every record.
    let (head, body) = get(&server, "/v1/stats/duration");
    assert!(head.starts_with("HTTP/1.1 400"), "fold not 400: {head:.80}");
    let payload: Value = serde_json::from_str(&body).expect("fold error parses");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("query deadline exceeded: "),
        "fold error wrong: {body:.200}"
    );

    // The session path — the both-locks attribute union, which bypasses the
    // ordinary query path entirely. Every seeded span carries the session,
    // so resolving it decodes the corpus; this 400 is the proof the budget
    // reaches that path too.
    let (head, body) = get(&server, "/v1/sessions/night-shift");
    assert!(
        head.starts_with("HTTP/1.1 400"),
        "session not 400: {head:.80}"
    );
    let payload: Value = serde_json::from_str(&body).expect("session error parses");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("query deadline exceeded: "),
        "session error wrong: {body:.200}"
    );
}

/// One `tools/call` over `POST /v1/mcp`, returning the parsed JSON-RPC
/// response.
fn mcp_call(server: &Server, tool: &str, arguments: Value) -> Value {
    let message = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
        },
    })
    .to_string();
    let mut stream = server.connect();
    write!(
        stream,
        "POST /v1/mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        message.len()
    )
    .expect("mcp head writes");
    stream.write_all(message.as_bytes()).expect("mcp writes");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("mcp reads");
    let text = String::from_utf8_lossy(&response).into_owned();
    assert!(text.starts_with("HTTP/1.1 200"), "not a 200: {text:.80}");
    text.split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str(body).ok())
        .expect("JSON-RPC response parses")
}

/// The narrowing guidance a tool refusal must carry, wherever it came from.
fn assert_narrowing_guidance(payload: &Value) {
    let result = &payload["result"];
    assert_eq!(
        result["isError"],
        Value::Bool(true),
        "not a tool error: {payload}"
    );
    let guidance = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        guidance.contains("compute budget") && guidance.contains("Narrow the window"),
        "guidance missing: {guidance}"
    );
}

#[test]
fn a_trace_lookup_past_its_budget_answers_400_and_is_counted() {
    let dir = test_dir("trace-400");
    let server = Server::spawn(&dir, &["--query-deadline-ms", "1", "--flush-spans", "5000"]);
    // Every span in ONE trace: the lookup's cost is the posting width, and
    // here the posting is the corpus, decoded under both engine locks.
    seed_single_trace(&server, CORPUS);

    let (head, body) = get(&server, "/v1/traces/mega");
    assert!(head.starts_with("HTTP/1.1 400"), "not a 400: {head:.80}");
    let payload: Value = serde_json::from_str(&body).expect("error body parses");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("query deadline exceeded: "),
        "wrong error: {body:.200}"
    );

    let (_, metrics) = get(&server, "/v1/metrics");
    let counted = metric(&metrics, "traza_query_deadline_exceeded_total").unwrap_or(0);
    assert!(counted >= 1, "refusal was not counted:\n{metrics:.400}");
}

#[test]
fn mcp_search_spans_reports_an_exhausted_budget_as_a_tool_error() {
    let dir = test_dir("mcp-tool-error");
    let server = Server::spawn(
        &dir,
        &[
            "--mcp",
            "--no-content-index",
            "--query-deadline-ms",
            "1",
            "--flush-spans",
            "5000",
        ],
    );
    seed(&server, CORPUS);
    let payload = mcp_call(
        &server,
        "search_spans",
        json!({"content": "zzznotpresent", "limit": 10}),
    );
    assert_narrowing_guidance(&payload);
}

#[test]
fn mcp_tools_share_the_narrowing_guidance_through_the_conversion_chokepoint() {
    // Deliberately NOT search_spans: that tool curates its own errors, so
    // only a tool that leans on the shared `From<Error>` conversion proves
    // the guidance lives at the chokepoint rather than in one handler.
    // `get_session` resolves the session union — every seeded span carries
    // the session — so a 1 ms budget is reliably exhausted mid-decode.
    let dir = test_dir("mcp-chokepoint");
    let server = Server::spawn(
        &dir,
        &["--mcp", "--query-deadline-ms", "1", "--flush-spans", "5000"],
    );
    seed(&server, CORPUS);
    let payload = mcp_call(&server, "get_session", json!({"session_id": "night-shift"}));
    assert_narrowing_guidance(&payload);
}

#[test]
fn export_is_exempt_and_streams_to_completion_under_a_one_ms_budget() {
    let dir = test_dir("export-exempt");
    let server = Server::spawn(&dir, &["--query-deadline-ms", "1", "--flush-spans", "5000"]);
    seed(&server, CORPUS);

    let started = Instant::now();
    let (_, body) = get(&server, "/v1/export");
    // A tripwire, not the oracle: the trailer below is the proof; this only
    // catches a stream that stalled without ending.
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "export took {:?}",
        started.elapsed()
    );
    assert!(
        body.contains("X-Traza-Export-Complete: true"),
        "export did not complete under the budget"
    );
    assert!(
        body.contains(&format!("X-Traza-Export-Count: {CORPUS}")),
        "export row count missing or short"
    );
}

#[test]
fn test_panic_zero_disables_the_route_like_every_other_knob() {
    // `TRAZA_TEST_PANIC=0` must mean OFF: every other knob reads `0` as
    // disabled, and a latch that read mere presence turned the one value
    // that says "off" everywhere else into "on" here. Sits with the other
    // env-latch hardening from the same review; the latch's positive half
    // lives in panic_guard.rs.
    let dir = test_dir("panic-zero");
    let server = Server::spawn_with_env(&dir, &[], &[("TRAZA_TEST_PANIC", "0")]);
    let (head, _) = get(&server, "/v1/test-panic");
    assert!(
        head.starts_with("HTTP/1.1 404"),
        "TRAZA_TEST_PANIC=0 left the route live: {head:.80}"
    );
}
