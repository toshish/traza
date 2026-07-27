//! The live tail streams in admission order, not event-time order.
//!
//! The distinction is the entire reason this surface exists, so the first test
//! below is written to fail against the design it replaced. Under event-time
//! paging a client tracks a `start_time_ns` watermark; a span that runs longer
//! than one poll interval starts BEFORE that watermark and arrives AFTER it, so
//! the server filters it out permanently. It is not lag. The span is in the
//! store, it matches the filter, and no later request will ever return it.
//!
//! `event_time_paging_still_loses_the_span_the_tail_delivers` holds both
//! mechanisms against the same two spans and asserts the difference directly,
//! so the guard cannot pass by accident if the tail ever quietly reverts to
//! ordering by timestamp.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::tail::TailRead;
use traza::{Config, Durability, Span, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-live-tail-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span(trace: &str, id: &str, start_ns: u64, end_ns: u64) -> Span {
    Span {
        trace_id: trace.to_owned(),
        span_id: id.to_owned(),
        parent_span_id: None,
        name: "op".to_owned(),
        start_time_ns: start_ns,
        end_time_ns: end_ns,
        status: "ok".to_owned(),
        service: "svc".to_owned(),
        attributes: Default::default(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Default::default(),
    }
}

fn store(label: &str) -> (Store, PathBuf) {
    let dir = test_dir(label);
    let config = Config {
        durability: Durability::Buffered,
        ..Config::default()
    };
    (Store::open(&dir, config).expect("store opens"), dir)
}

fn ids(read: &TailRead) -> Vec<String> {
    match read {
        TailRead::Batch { spans, .. } => spans.iter().map(|s| s.span_id.clone()).collect(),
        TailRead::Gap { .. } => panic!("unexpected gap"),
    }
}

fn cursor_of(read: &TailRead) -> traza::tail::TailCursor {
    match read {
        TailRead::Batch { cursor, .. } => *cursor,
        // A gap carries no position, by design — see `TailRead::Gap`.
        TailRead::Gap { .. } => panic!("a gap has no position to resume from"),
    }
}

#[test]
fn event_time_paging_still_loses_the_span_the_tail_delivers() {
    let (engine, _dir) = store("ordering");

    // `a` starts at 10s. A watching client sees it and its watermark moves to
    // 10s.
    engine
        .ingest_batch(vec![span("t1", "a", 10_000, 11_000)])
        .expect("ingest a");
    let first = engine
        .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
        .expect("tail");
    assert_eq!(ids(&first), ["a"]);
    let cursor = cursor_of(&first);

    // `b` STARTED at 5s and is only now finishing — a long-running operation
    // that was already in flight when `a` began. It is admitted second.
    engine
        .ingest_batch(vec![span("t2", "b", 5_000, 30_000)])
        .expect("ingest b");

    // Event-time paging: the client asks for everything at or after its
    // watermark. `b` started before it, so this is empty — permanently. This
    // assertion is the bug, reproduced.
    let missed = engine
        .query(&SpanFilter {
            since_ns: Some(10_000),
            ..SpanFilter::default()
        })
        .expect("query");
    assert!(
        !missed.iter().any(|span| span.span_id == "b"),
        "event-time paging is expected to miss `b`; if it does not, this test \
         no longer proves anything and needs rewriting"
    );

    // The store holds it, so nothing was lost on ingest.
    let everything = engine.query(&SpanFilter::default()).expect("query");
    assert!(everything.iter().any(|span| span.span_id == "b"));

    // Admission order delivers it. This is the line that fails against the
    // design this replaced.
    let second = engine
        .tail_after(
            Some(cursor),
            100,
            100,
            &SpanFilter::default(),
            Duration::ZERO,
        )
        .expect("tail");
    assert_eq!(
        ids(&second),
        ["b"],
        "a span that started before the watermark must still be delivered when it lands"
    );
}

#[test]
fn a_settled_cursor_never_replays() {
    let (engine, _dir) = store("settled");
    engine
        .ingest_batch(vec![
            span("t1", "a", 1_000, 2_000),
            span("t1", "b", 1_000, 2_000),
        ])
        .expect("ingest");

    let first = engine
        .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
        .expect("tail");
    assert_eq!(ids(&first).len(), 2);
    let mut cursor = cursor_of(&first);

    // Identical timestamps, which no watermark can separate — the burst that
    // made the previous implementation replay its first page forever.
    for _ in 0..5 {
        let read = engine
            .tail_after(
                Some(cursor),
                100,
                100,
                &SpanFilter::default(),
                Duration::ZERO,
            )
            .expect("tail");
        assert!(ids(&read).is_empty(), "settled cursor must be silent");
        cursor = cursor_of(&read);
    }
}

#[test]
fn a_filter_applies_to_the_stream() {
    let (engine, _dir) = store("filter");
    let mut failed = span("t1", "bad", 1_000, 2_000);
    failed.status = "error".to_owned();
    engine
        .ingest_batch(vec![span("t1", "good", 1_000, 2_000), failed])
        .expect("ingest");

    let read = engine
        .tail_after(
            None,
            100,
            100,
            &SpanFilter {
                status: Some("error".to_owned()),
                ..SpanFilter::default()
            },
            Duration::ZERO,
        )
        .expect("tail");
    assert_eq!(ids(&read), ["bad"]);
}

#[test]
fn falling_off_the_ring_reports_a_gap() {
    let dir = test_dir("gap");
    let engine = Store::open(
        &dir,
        Config {
            durability: Durability::Buffered,
            tail_ring_spans: 4,
            ..Config::default()
        },
    )
    .expect("store opens");

    engine
        .ingest_batch(vec![span("t1", "a", 1_000, 2_000)])
        .expect("ingest");
    let cursor = cursor_of(
        &engine
            .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
            .expect("tail"),
    );

    for index in 0..8 {
        engine
            .ingest_batch(vec![span("t1", &format!("x{index}"), 3_000, 4_000)])
            .expect("ingest");
    }

    // A gap is reported rather than entries being silently skipped: the client
    // can recover from being told, and cannot recover from not being told.
    let read = engine
        .tail_after(
            Some(cursor),
            100,
            100,
            &SpanFilter::default(),
            Duration::ZERO,
        )
        .expect("tail");
    assert!(
        matches!(read, TailRead::Gap { .. }),
        "a cursor older than the ring must gap, not skip"
    );
}

#[test]
fn a_waiting_subscriber_is_woken_by_an_ingest() {
    use std::sync::Arc;

    let (engine, _dir) = store("wake");
    let engine = Arc::new(engine);
    let head = engine.tail_head().expect("head");

    let writer = Arc::clone(&engine);
    let ingest = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        writer
            .ingest_batch(vec![span("t1", "late", 1_000, 2_000)])
            .expect("ingest");
    });

    // A five-second budget that must not be spent: the point is that the span
    // is pushed to the waiting subscriber, not that the deadline eventually
    // expires and it re-reads.
    let started = Instant::now();
    let read = engine
        .tail_after(
            Some(head),
            0,
            100,
            &SpanFilter::default(),
            Duration::from_secs(5),
        )
        .expect("tail");
    let waited = started.elapsed();
    ingest.join().expect("ingest thread");

    assert_eq!(ids(&read), ["late"]);
    assert!(
        waited < Duration::from_secs(4),
        "delivery must follow the ingest, not the timeout (waited {waited:?})"
    );
}

// ---------------------------------------------------------------------------
// The HTTP surface.
// ---------------------------------------------------------------------------

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path) -> Self {
        Self::spawn_with(data_dir, &[])
    }

    fn spawn_with(data_dir: &Path, extra: &[&str]) -> Self {
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
            .env("TRAZA_SOCKET_TIMEOUT_MS", "30000")
            .stderr(Stdio::piped());
        for argument in extra {
            command.arg(argument);
        }
        let mut child = command.spawn().expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = BufReader::new(stderr);
        let port = {
            let mut line = String::new();
            let mut startup = String::new();
            loop {
                line.clear();
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
                        .set_read_timeout(Some(Duration::from_secs(10)))
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

    fn ingest(&self, spans: Value) {
        let body = spans.to_string();
        let mut stream = self.connect();
        write!(
            stream,
            "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write ingest");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("read ingest");
        assert!(
            answer.starts_with("HTTP/1.1 2"),
            "ingest rejected: {answer}"
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the response head, then decodes chunked SSE frames on demand.
struct Stream {
    reader: BufReader<TcpStream>,
}

impl Stream {
    fn open(server: &Server, query: &str) -> (String, Self) {
        let mut socket = server.connect();
        write!(
            socket,
            "GET /v1/tail{query} HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n"
        )
        .expect("write request");
        socket.flush().expect("flush");
        let mut reader = BufReader::new(socket);
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let read = reader.read(&mut byte).expect("header byte");
            assert_ne!(read, 0, "closed mid-header");
            head.push(byte[0]);
        }
        (
            String::from_utf8(head).expect("utf-8 head"),
            Self { reader },
        )
    }

    /// One chunked frame, as text. Blocks until the server sends one.
    fn frame(&mut self) -> String {
        let mut size_line = String::new();
        self.reader.read_line(&mut size_line).expect("chunk size");
        let size = usize::from_str_radix(size_line.trim(), 16).expect("hex chunk size");
        let mut body = vec![0_u8; size];
        self.reader.read_exact(&mut body).expect("chunk body");
        let mut terminator = [0_u8; 2];
        self.reader.read_exact(&mut terminator).expect("chunk CRLF");
        String::from_utf8(body).expect("utf-8 frame")
    }

    /// The next `event: spans` frame's payload, skipping heartbeats.
    fn next_spans(&mut self) -> Value {
        for _ in 0..20 {
            let frame = self.frame();
            if let Some(data) = frame.strip_prefix("event: spans\ndata: ") {
                return serde_json::from_str(data.trim_end()).expect("json payload");
            }
        }
        panic!("no spans frame within 20 frames");
    }
}

#[test]
fn the_stream_delivers_a_span_that_started_before_the_one_already_seen() {
    let dir = test_dir("http-order");
    let server = Server::spawn(&dir);

    server.ingest(json!([{
        "trace_id": "t1", "span_id": "a", "name": "op", "service": "svc",
        "start_time_ns": 10_000_u64, "end_time_ns": 11_000_u64, "status": "ok",
    }]));

    let (head, mut stream) = Stream::open(&server, "?backfill=100");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );

    let opening = stream.next_spans();
    let seen: Vec<&str> = opening["spans"]
        .as_array()
        .expect("spans array")
        .iter()
        .map(|span| span["span_id"].as_str().expect("span_id"))
        .collect();
    assert_eq!(seen, ["a"]);

    // Started earlier, lands later — invisible to a `since=` watermark.
    server.ingest(json!([{
        "trace_id": "t2", "span_id": "b", "name": "op", "service": "svc",
        "start_time_ns": 5_000_u64, "end_time_ns": 30_000_u64, "status": "ok",
    }]));

    let next = stream.next_spans();
    let arrived: Vec<&str> = next["spans"]
        .as_array()
        .expect("spans array")
        .iter()
        .map(|span| span["span_id"].as_str().expect("span_id"))
        .collect();
    assert_eq!(
        arrived,
        ["b"],
        "the late span must reach a connected client"
    );
    assert!(next["cursor"].is_string(), "every frame carries a position");
}

#[test]
fn an_event_time_bound_is_refused_rather_than_ignored() {
    let dir = test_dir("http-since");
    let server = Server::spawn(&dir);
    let (head, _stream) = Stream::open(&server, "?since=1000");
    assert!(
        head.starts_with("HTTP/1.1 400"),
        "a tail cannot honour an event-time window, and must say so: {head}"
    );
}

#[test]
fn a_malformed_cursor_is_refused() {
    let dir = test_dir("http-cursor");
    let server = Server::spawn(&dir);
    let (head, _stream) = Stream::open(&server, "?cursor=not-a-cursor");
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[test]
fn a_filter_narrows_the_stream() {
    let dir = test_dir("http-filter");
    let server = Server::spawn(&dir);
    server.ingest(json!([
        {"trace_id": "t1", "span_id": "ok1", "name": "op", "service": "svc",
         "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "ok"},
        {"trace_id": "t1", "span_id": "bad1", "name": "op", "service": "svc",
         "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "error"},
    ]));

    let (_head, mut stream) = Stream::open(&server, "?backfill=100&status=error");
    let opening = stream.next_spans();
    let seen: Vec<&str> = opening["spans"]
        .as_array()
        .expect("spans array")
        .iter()
        .map(|span| span["span_id"].as_str().expect("span_id"))
        .collect();
    assert_eq!(seen, ["bad1"]);
}

fn server_metrics(server: &Server) -> Value {
    let mut socket = server.connect();
    write!(
        socket,
        "GET /v1/metrics.json HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    let mut answer = String::new();
    socket.read_to_string(&mut answer).expect("read");
    let body = answer.split("\r\n\r\n").nth(1).unwrap_or_default();
    serde_json::from_str(body).expect("json metrics")
}

#[test]
fn a_stream_is_counted_but_never_timed() {
    // A tail lasts as long as the client watches. Recording that as a request
    // latency would put hours into the histogram every dashboard tab derives
    // its percentiles from.
    let dir = test_dir("http-metrics");
    let server = Server::spawn(&dir);
    server.ingest(json!([{
        "trace_id": "t1", "span_id": "a", "name": "op", "service": "svc",
        "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "ok",
    }]));

    {
        let (_head, mut stream) = Stream::open(&server, "?backfill=100");
        stream.next_spans();
        // Hold it open long enough that a timed class would record it as a
        // clearly non-zero duration.
        std::thread::sleep(Duration::from_millis(300));
    }

    // The handler is parked on the ring waiting for spans, so it learns the
    // client is gone only when it next tries to write. Ingesting is what makes
    // it try — the same way a real disconnect is discovered.
    let stream_class = {
        let mut found = Value::Null;
        for index in 0..30 {
            server.ingest(json!([{
                "trace_id": "t1", "span_id": format!("w{index}"), "name": "op",
                "service": "svc", "start_time_ns": 1_000_u64,
                "end_time_ns": 2_000_u64, "status": "ok",
            }]));
            let stats = server_metrics(&server);
            if stats["by_class"]["stream"]["count"].as_u64().unwrap_or(0) >= 1 {
                found = stats["by_class"]["stream"].clone();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        found
    };
    assert!(
        stream_class["count"].as_u64().unwrap_or(0) >= 1,
        "the stream is counted once its handler notices the disconnect: {stream_class}"
    );
    assert_eq!(
        stream_class["max_ns"].as_u64(),
        Some(0),
        "a stream's duration must never enter the latency histogram: {stream_class}"
    );
}

#[test]
fn the_library_refuses_an_event_time_window_rather_than_applying_it() {
    // The bug this guards: `tail_after` documented `since_ns`/`until_ns` as
    // ignored while handing the whole filter to `span_matches`, which applies
    // both. A span starting before the window was dropped AND the cursor
    // advanced past it, so it could never be delivered — the original
    // permanent-loss bug, reproduced for every library caller behind a doc
    // comment saying it could not happen.
    let (engine, _dir) = store("library-window");
    engine
        .ingest_batch(vec![span("t1", "early", 5_000, 30_000)])
        .expect("ingest");

    for bounded in [
        SpanFilter {
            since_ns: Some(10_000),
            ..SpanFilter::default()
        },
        SpanFilter {
            until_ns: Some(1_000),
            ..SpanFilter::default()
        },
    ] {
        let refused = engine.tail_after(None, 100, 100, &bounded, Duration::ZERO);
        assert!(
            matches!(refused, Err(traza::Error::UnsupportedFilter(_))),
            "an event-time bound must be refused, not silently applied"
        );
    }

    // Without the bound the same span is delivered, which is what makes the
    // refusal a refusal rather than the store simply having nothing.
    let allowed = engine
        .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
        .expect("tail");
    assert_eq!(ids(&allowed), ["early"]);
}

#[test]
fn a_gap_restarts_the_stream_without_replaying_or_duplicating() {
    // The gap contract, end to end, by the route that actually produces one:
    // a client disconnects, the ring turns over while it is away, and it
    // reconnects with a position the server no longer holds.
    //
    // The previous design answered that with the ring's FLOOR, replayed every
    // retained entry from there, and told the client to "backfill what was
    // dropped" from an event-time query it could not express. The overlap
    // between the two showed the same span twice.
    let dir = test_dir("http-gap");
    let server = Server::spawn_with(&dir, &["--tail-ring-spans", "4"]);

    server.ingest(json!([{
        "trace_id": "t1", "span_id": "first", "name": "op", "service": "svc",
        "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "ok",
    }]));

    let stale = {
        let (_head, mut stream) = Stream::open(&server, "?backfill=100");
        let opening = stream.next_spans();
        assert_eq!(opening["spans"].as_array().expect("spans").len(), 1);
        opening["cursor"].as_str().expect("cursor").to_owned()
    };

    // Away long enough that the ring turns over several times.
    for index in 0..24 {
        server.ingest(json!([{
            "trace_id": "t1", "span_id": format!("x{index}"), "name": "op",
            "service": "svc", "start_time_ns": 3_000_u64 + index as u64,
            "end_time_ns": 4_000_u64, "status": "ok",
        }]));
    }

    let (_head, mut stream) = Stream::open(&server, &format!("?cursor={stale}"));

    // The first frame must be the gap, and it must offer no position.
    let payload = loop {
        let frame = stream.frame();
        if let Some(data) = frame.strip_prefix("event: gap\ndata: ") {
            break serde_json::from_str::<Value>(data.trim_end()).expect("json");
        }
        assert!(
            frame.starts_with(':'),
            "nothing but a heartbeat may precede the gap: {frame}"
        );
    };
    assert!(
        payload.get("cursor").is_none(),
        "a gap must not offer a position to resume from: {payload}"
    );
    assert_eq!(
        payload["missed"].as_u64(),
        Some(20),
        "25 admitted, this subscriber had seen 1, and 4 are still retained — \
         so 20 passed through the ring while it was away: {payload}"
    );

    // What follows is one fresh backlog, bounded by what the ring holds — not
    // a replay from the floor, and with no key repeated.
    let rebuild = stream.next_spans();
    let keys: Vec<String> = rebuild["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .map(|span| format!("{}/{}", span["trace_id"], span["span_id"]))
        .collect();
    let distinct: std::collections::HashSet<&String> = keys.iter().collect();
    assert_eq!(
        keys.len(),
        distinct.len(),
        "the rebuild must not repeat a span: {keys:?}"
    );
    assert!(
        !keys.is_empty() && keys.len() <= 4,
        "bounded by what the ring retains, not by what was lost: {keys:?}"
    );
}

#[test]
fn the_ring_reports_its_residency_and_both_bounds() {
    // The tail is the only structure that holds whole spans indefinitely, so
    // "is this why the process is large" has to be answerable without a
    // profiler.
    let dir = test_dir("http-usage");
    let server = Server::spawn_with(
        &dir,
        &["--tail-ring-spans", "16", "--tail-ring-bytes", "4096"],
    );
    server.ingest(json!([{
        "trace_id": "t1", "span_id": "a", "name": "op", "service": "svc",
        "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "ok",
    }]));

    let stats = server_metrics(&server);
    let ring = &stats["tail_ring"];
    assert_eq!(ring["max_spans"].as_u64(), Some(16));
    assert_eq!(ring["max_bytes"].as_u64(), Some(4096));
    assert_eq!(ring["spans"].as_u64(), Some(1));
    assert!(
        ring["bytes"].as_u64().unwrap_or(0) > 0,
        "residency is measured, not assumed: {ring}"
    );
}

#[test]
#[cfg(unix)]
fn a_failed_ingest_never_reaches_the_tail() {
    // "Admitted" has to mean acknowledged. The ring used to be published
    // straight after the write-buffer upsert — before the fsync that makes the
    // batch survivable, and before the synchronous seal that
    // `Durability::Flushed` promises — so a tail could show a span whose
    // ingest then returned an error, or which a crash a millisecond later
    // erased. A live view may be bounded and may admit gaps; it may not show
    // data the store never accepted.
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("failed-ingest");
    let engine = Store::open(
        &dir,
        Config {
            // Every batch seals, and the seal has to finish before the
            // acknowledgement — so making the seal fail fails the ingest.
            durability: Durability::Flushed,
            flush_spans: 1,
            ..Config::default()
        },
    )
    .expect("store opens");

    // A successful ingest first, to prove the tail is working at all.
    engine
        .ingest_batch(vec![span("t1", "accepted", 1_000, 2_000)])
        .expect("ingest");
    let cursor = cursor_of(
        &engine
            .tail_after(None, 100, 100, &SpanFilter::default(), Duration::ZERO)
            .expect("tail"),
    );

    let mut locked = std::fs::metadata(&dir).expect("metadata").permissions();
    locked.set_mode(0o555);
    std::fs::set_permissions(&dir, locked).expect("lock the directory");

    let refused = engine.ingest_batch(vec![span("t1", "rejected", 3_000, 4_000)]);

    let mut unlocked = std::fs::metadata(&dir).expect("metadata").permissions();
    unlocked.set_mode(0o755);
    std::fs::set_permissions(&dir, unlocked).expect("unlock the directory");

    // Running as root ignores the permission bits, so the fault never happens
    // and the test has nothing to observe. Say so rather than pass silently.
    assert!(
        refused.is_err(),
        "expected the seal to fail on a read-only directory; if this is running \
         as root the fault cannot be injected and the guard proves nothing"
    );

    let after = engine
        .tail_after(
            Some(cursor),
            100,
            100,
            &SpanFilter::default(),
            Duration::ZERO,
        )
        .expect("tail");
    assert!(
        ids(&after).is_empty(),
        "a span whose ingest failed must never appear in the tail: {:?}",
        ids(&after)
    );
}

#[test]
fn a_gap_honours_an_explicit_request_for_no_backlog() {
    // `backfill=0` means "I want nothing but what arrives from now". After a
    // gap the server used to substitute its own default, so a subscriber that
    // had explicitly asked for no history was handed a screenful of retained
    // spans it never wanted — and, worse, spans that predate the break it was
    // just told about.
    let dir = test_dir("http-gap-zero-backfill");
    let server = Server::spawn_with(&dir, &["--tail-ring-spans", "4"]);

    server.ingest(json!([{
        "trace_id": "t1", "span_id": "first", "name": "op", "service": "svc",
        "start_time_ns": 1_000_u64, "end_time_ns": 2_000_u64, "status": "ok",
    }]));

    let stale = {
        let (_head, mut stream) = Stream::open(&server, "?backfill=100");
        let opening = stream.next_spans();
        opening["cursor"].as_str().expect("cursor").to_owned()
    };

    // Turn the ring over so the position is unusable.
    for index in 0..24 {
        server.ingest(json!([{
            "trace_id": "t1", "span_id": format!("x{index}"), "name": "op",
            "service": "svc", "start_time_ns": 3_000_u64 + index as u64,
            "end_time_ns": 4_000_u64, "status": "ok",
        }]));
    }

    let (_head, mut stream) = Stream::open(&server, &format!("?cursor={stale}&backfill=0"));

    // Consume the gap frame.
    loop {
        let frame = stream.frame();
        if frame.starts_with("event: gap") {
            break;
        }
        assert!(
            frame.starts_with(':'),
            "unexpected frame before the gap: {frame}"
        );
    }

    // Now a fresh span arrives. It — and nothing before it — must be what the
    // subscriber receives.
    server.ingest(json!([{
        "trace_id": "t1", "span_id": "after-the-gap", "name": "op",
        "service": "svc", "start_time_ns": 9_000_u64, "end_time_ns": 9_500_u64,
        "status": "ok",
    }]));

    // Collect every span the stream delivers until the new one shows up. The
    // frame immediately after a gap is legitimately empty — it carries the
    // subscriber's new position — so the property under test is not "the next
    // frame has one span" but "no retained span is ever delivered".
    let mut delivered: Vec<String> = Vec::new();
    for _ in 0..20 {
        let batch = stream.next_spans();
        for span in batch["spans"].as_array().expect("spans") {
            delivered.push(span["span_id"].as_str().expect("id").to_owned());
        }
        if delivered.iter().any(|id| id == "after-the-gap") {
            break;
        }
    }
    assert_eq!(
        delivered,
        vec!["after-the-gap".to_owned()],
        "backfill=0 must stay zero across a gap; got {delivered:?}"
    );
}
