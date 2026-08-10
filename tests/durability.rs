//! The acknowledgement contract, proven by killing the process.
//!
//! A durability mode is a promise about what a 200 means, and the only
//! convincing test of that promise is SIGKILL: no unwinding, no destructors,
//! no flush on the way out. Each mode is held to exactly what it claims —
//! `wal` and `flushed` lose nothing acknowledged, and `buffered` is verified
//! to be lossy rather than accidentally durable.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, durability: &str) -> Self {
        Self::spawn_with(data_dir, durability, &[])
    }

    fn spawn_with(data_dir: &Path, durability: &str, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        let mut child = command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg(durability)
            // Keep the buffer from sealing on its own: the point is to test
            // what survives WITHOUT a segment flush.
            .arg("--flush-spans")
            .arg("1000000")
            .env_remove("TRAZA_TOKENS")
            .args(extra)
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

    /// SIGKILL: the process gets no chance to flush anything on the way out.
    fn kill_hard(&mut self) {
        self.child.kill().expect("kill");
        self.child.wait().expect("reap");
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        request_to(self.port, method, target, body)
    }
}

/// One request against a port. Free-standing so concurrent clients do not have
/// to own a `Server`.
fn request_to(port: u16, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
    {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 100 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect: {error}"),
                }
            }
        };
        let length = encoded.as_ref().map_or(0, Vec::len);
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
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
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("traza-dur-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A batch of `count` spans, all in one trace, tagged by `marker`.
fn batch(marker: &str, count: usize) -> Value {
    let spans: Vec<Value> = (0..count)
        .map(|index| {
            json!({
                "trace_id": format!("trace-{marker}"),
                "span_id": format!("span-{index:05}"),
                "name": "acknowledged",
                "service": "ingest",
                "start_time_ns": 1_000_000_000u64 + index as u64,
                "end_time_ns": 1_000_000_500u64 + index as u64,
                "attributes": {"marker": marker}
            })
        })
        .collect();
    Value::Array(spans)
}

fn surviving_spans(dir: &Path, marker: &str) -> usize {
    // A fresh server over the same directory performs the real recovery path.
    let server = Server::spawn(dir, "wal");
    let (status, body) = server.request(
        "GET",
        &format!("/v1/spans?attr.marker={marker}&limit=100000"),
        None,
    );
    assert_eq!(status, 200, "recovered store serves queries: {body}");
    let count = body["spans"].as_array().map(Vec::len).unwrap_or(0);
    let mut server = server;
    server.kill_hard();
    count
}

#[test]
fn wal_mode_loses_nothing_it_acknowledged() {
    let dir = test_dir("wal-kill");
    let mut server = Server::spawn(&dir, "wal");

    let (status, body) = server.request("POST", "/v1/spans", Some(&batch("wal", 500)));
    assert_eq!(status, 200, "ingest acknowledged: {body}");
    assert_eq!(body["accepted"], 500);
    assert_eq!(
        body["durability"], "wal",
        "the response states what the acknowledgement means"
    );

    // Nothing was flushed: the spans live only in the log and memory.
    let (_, stats) = server.request("GET", "/v1/stats", None);
    assert_eq!(stats["segment_count"], 0, "no segment sealed yet: {stats}");
    assert!(
        stats["wal_bytes"].as_u64().unwrap_or(0) > 0,
        "the log holds the acknowledged batch: {stats}"
    );

    server.kill_hard();

    assert_eq!(
        surviving_spans(&dir, "wal"),
        500,
        "every acknowledged span survives SIGKILL in wal mode"
    );
}

#[test]
fn flushed_mode_acknowledges_only_sealed_spans() {
    let dir = test_dir("flushed-kill");
    let mut server = Server::spawn(&dir, "flushed");

    let (status, body) = server.request("POST", "/v1/spans", Some(&batch("flushed", 100)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["durability"], "flushed");

    // The contract is stronger here: acknowledged means already in a segment.
    let (_, stats) = server.request("GET", "/v1/stats", None);
    assert!(
        stats["segment_count"].as_u64().unwrap_or(0) >= 1,
        "acknowledgement implies a sealed segment: {stats}"
    );
    assert_eq!(stats["buffered_records"], 0, "nothing left in memory");

    server.kill_hard();

    assert_eq!(
        surviving_spans(&dir, "flushed"),
        100,
        "every acknowledged span survives SIGKILL in flushed mode"
    );
}

#[test]
fn buffered_mode_is_lossy_exactly_as_documented() {
    let dir = test_dir("buffered-kill");
    let mut server = Server::spawn(&dir, "buffered");

    let (status, body) = server.request("POST", "/v1/spans", Some(&batch("buffered", 200)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["durability"], "buffered",
        "the response does not imply more than the mode provides"
    );
    let (_, stats) = server.request("GET", "/v1/stats", None);
    assert_eq!(
        stats["wal_bytes"], 0,
        "buffered mode writes no log: {stats}"
    );

    server.kill_hard();

    // This is the documented behaviour, not a bug — and it is exactly why it
    // is not the default.
    assert_eq!(
        surviving_spans(&dir, "buffered"),
        0,
        "buffered mode loses unflushed writes, as documented"
    );
}

#[test]
fn a_flush_makes_buffered_writes_survive_too() {
    // Even the lossy mode keeps what it sealed: the loss window is bounded by
    // the flush, not unbounded.
    let dir = test_dir("buffered-flush");
    let mut server = Server::spawn(&dir, "buffered");

    let (status, _) = server.request("POST", "/v1/spans", Some(&batch("sealed", 50)));
    assert_eq!(status, 200);
    let (status, _) = server.request("POST", "/v1/flush", None);
    assert_eq!(status, 200);
    server.kill_hard();

    assert_eq!(surviving_spans(&dir, "sealed"), 50);
}

#[test]
fn recovery_replays_the_newest_version_of_a_re_ingested_span() {
    // Last-write-wins has to survive recovery: the log is ordered, so replay
    // must reproduce the same winner the buffer had.
    let dir = test_dir("wal-lww");
    let mut server = Server::spawn(&dir, "wal");

    let span = |name: &str| {
        json!([{
            "trace_id": "t", "span_id": "s", "name": name, "service": "ingest",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
            "attributes": {"marker": "lww"}
        }])
    };
    for name in ["first", "second", "third"] {
        let (status, _) = server.request("POST", "/v1/spans", Some(&span(name)));
        assert_eq!(status, 200);
    }
    server.kill_hard();

    let server = Server::spawn(&dir, "wal");
    let (status, body) = server.request("GET", "/v1/traces/t", None);
    assert_eq!(status, 200, "{body}");
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 1, "one primary key, one span: {body}");
    assert_eq!(spans[0]["name"], "third", "the newest version wins: {body}");
    let mut server = server;
    server.kill_hard();
}

#[test]
fn the_log_is_reclaimed_once_its_spans_are_sealed() {
    let dir = test_dir("wal-reclaim");
    let mut server = Server::spawn(&dir, "wal");

    let (status, _) = server.request("POST", "/v1/spans", Some(&batch("reclaim", 300)));
    assert_eq!(status, 200);
    let (_, before) = server.request("GET", "/v1/stats", None);
    assert!(
        before["wal_bytes"].as_u64().unwrap_or(0) > 0,
        "log holds the batch before the flush: {before}"
    );

    let (status, _) = server.request("POST", "/v1/flush", None);
    assert_eq!(status, 200);

    let (_, after) = server.request("GET", "/v1/stats", None);
    assert_eq!(
        after["wal_bytes"], 0,
        "a sealed segment supersedes the log, which is then reclaimed: {after}"
    );
    assert_eq!(after["segment_count"], 1);

    server.kill_hard();
    // Reclamation must not have cost us the data.
    assert_eq!(surviving_spans(&dir, "reclaim"), 300);
}

#[test]
fn concurrent_acknowledged_batches_all_survive() {
    // Group commit is the risky part: one fsync covers many batches, so a
    // bookkeeping error would lose writes that were already acknowledged.
    let dir = test_dir("wal-concurrent");
    let mut server = Server::spawn(&dir, "wal");
    let port = server.port;

    let mut handles = Vec::new();
    for worker in 0..8 {
        handles.push(std::thread::spawn(move || {
            for round in 0..10 {
                let marker = format!("c{worker}");
                let spans: Vec<Value> = (0..10)
                    .map(|index| {
                        json!({
                            "trace_id": format!("trace-{worker}"),
                            "span_id": format!("span-{round}-{index}"),
                            "name": "acknowledged", "service": "ingest",
                            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                            "attributes": {"marker": marker}
                        })
                    })
                    .collect();
                let (status, _) = request_to(port, "POST", "/v1/spans", Some(&Value::Array(spans)));
                assert_eq!(status, 200);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker");
    }
    server.kill_hard();

    let total: usize = (0..8)
        .map(|worker| surviving_spans(&dir, &format!("c{worker}")))
        .sum();
    assert_eq!(
        total, 800,
        "every batch acknowledged under group commit survives SIGKILL"
    );
}

#[test]
fn a_crash_during_a_seal_loses_nothing_it_acknowledged() {
    // Sealing runs off the writer lock, which opens a window the old design
    // did not have: a segment can be fsynced, renamed and visible while the
    // log records it supersedes are still on disk and the buffer still holds
    // the spans. A crash anywhere in there must recover to the same store.
    //
    // The threshold is deliberately low, so seals run continuously and the
    // SIGKILL below lands inside one rather than between them. Both directions
    // are failures: losing an acknowledged span, or recovering a duplicate of
    // one because the segment and the replayed log disagreed about identity.
    // The kill has to land WHILE a seal is running. Waiting for the clients to
    // finish would not test anything: a seal runs on the thread whose batch
    // triggered it, so by the time the last request is answered every seal it
    // started has already published. The clients are given far more work than
    // they can finish and the server is killed underneath them.
    const PER_BATCH: usize = 400;
    let dir = test_dir("seal-crash");
    // Small threshold, fat spans: seals are frequent AND each one is real
    // work, so a kill at an arbitrary instant has a good chance of landing
    // inside one. Compaction is off — it would merge segments underneath the
    // measurement and add nothing to what is being tested.
    let mut server = Server::spawn_with(
        &dir,
        "wal",
        &["--flush-spans", "2000", "--compaction-fanout", "0"],
    );
    let port = server.port;

    // The kill is triggered by PROGRESS, not by a stopwatch. A fixed sleep made
    // the precondition below depend on how loaded the machine happened to be
    // while the rest of the suite ran beside it.
    const BATCHES_BEFORE_KILL: usize = 60; // 24,000 spans: about a dozen seals
    let progress = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for worker in 0..4 {
        let progress = Arc::clone(&progress);
        handles.push(std::thread::spawn(move || {
            acknowledged_batches(port, worker, 100_000, PER_BATCH, 256, Some(progress))
        }));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while progress.load(Ordering::Acquire) < BATCHES_BEFORE_KILL
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    server.kill_hard();

    let acknowledged: usize = handles
        .into_iter()
        .map(|handle| handle.join().expect("worker"))
        .sum();
    assert!(
        acknowledged >= BATCHES_BEFORE_KILL,
        "the run must have acknowledged enough to have sealed several times, \
         got {acknowledged} batches"
    );

    // ONE recovery serving all four queries. Recovery is the expensive half of
    // this test, and `/v1/stats` cannot answer it: it reports PHYSICAL records,
    // so a span that recovery finds in both a published segment and the
    // replayed log would count twice and hide a loss somewhere else. A query
    // resolves the primary key.
    let recovered = Server::spawn(&dir, "wal");
    let survived: usize = (0..4)
        .map(|worker| {
            let (status, body) = request_to(
                recovered.port,
                "GET",
                &format!("/v1/spans?attr.marker=ka{worker}&limit=1000000"),
                None,
            );
            assert_eq!(status, 200, "the recovered store answers: {body}");
            body["spans"].as_array().map(Vec::len).unwrap_or(0)
        })
        .sum();
    let mut recovered = recovered;
    recovered.kill_hard();

    assert!(
        survived >= acknowledged * PER_BATCH,
        "{survived} spans survived a SIGKILL taken mid-seal, but {} were \
         acknowledged: the log was reclaimed before the segment that \
         supersedes it was durable",
        acknowledged * PER_BATCH
    );
}

/// Sends many requests down ONE connection, the way a real ingest client does
/// now that connections persist. Returns how many were acknowledged 200.
///
/// Written out rather than reusing `request_to` because the whole point is the
/// connection reuse: `request_to` opens a fresh socket per request and so
/// cannot exercise the path where a response and the next request share a
/// buffer.
fn acknowledged_over_one_connection(port: u16, worker: usize, rounds: usize) -> usize {
    acknowledged_batches(port, worker, rounds, 10, 0, None)
}

/// [`acknowledged_over_one_connection`] with the batch shape spelled out.
///
/// `per_batch` and `detail_bytes` exist because how much a seal has to write
/// decides how wide the window a mid-seal crash has to land in is. Ten empty
/// spans per batch seals in microseconds and tests nothing about that window.
///
/// `progress` counts acknowledgements as they happen, so a caller can kill the
/// server after a known amount of work rather than after a guessed interval.
fn acknowledged_batches(
    port: u16,
    worker: usize,
    rounds: usize,
    per_batch: usize,
    detail_bytes: usize,
    progress: Option<Arc<AtomicUsize>>,
) -> usize {
    let mut stream = {
        let mut attempt = 0;
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => break stream,
                Err(_) if attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect: {error}"),
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let mut acknowledged = 0;
    let mut leftover: Vec<u8> = Vec::new();
    let detail = "x".repeat(detail_bytes);
    for round in 0..rounds {
        let spans: Vec<Value> = (0..per_batch)
            .map(|index| {
                json!({
                    "trace_id": format!("ka-trace-{worker}"),
                    "span_id": format!("span-{round}-{index}"),
                    "name": "acknowledged", "service": "ingest",
                    "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                    "attributes": {"marker": format!("ka{worker}"), "detail": detail}
                })
            })
            .collect();
        let body = serde_json::to_vec(&Value::Array(spans)).expect("encodes");
        let head = format!(
            "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&body).is_err() {
            break;
        }
        // Read exactly one response, keeping any surplus for the next round.
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            if let Some(position) = leftover.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return acknowledged,
                Ok(read) => leftover.extend_from_slice(&chunk[..read]),
            }
        };
        let header = String::from_utf8_lossy(&leftover[..header_end]).into_owned();
        let length: usize = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);
        while leftover.len() < header_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return acknowledged,
                Ok(read) => leftover.extend_from_slice(&chunk[..read]),
            }
        }
        leftover.drain(..header_end + length);
        if header.starts_with("HTTP/1.1 200") {
            acknowledged += 1;
            if let Some(progress) = &progress {
                progress.fetch_add(1, Ordering::Release);
            }
        }
    }
    acknowledged
}

#[test]
fn concurrent_keep_alive_clients_lose_nothing_they_were_promised() {
    // Persistent connections changed the shape of concurrent ingest: many
    // batches now arrive down one socket, and a response and the next request
    // share a read buffer. A framing slip there would corrupt or drop an
    // acknowledged batch, and only a count taken after SIGKILL would show it.
    let dir = test_dir("wal-keepalive");
    let mut server = Server::spawn(&dir, "wal");
    let port = server.port;

    let mut handles = Vec::new();
    for worker in 0..8 {
        handles.push(std::thread::spawn(move || {
            acknowledged_over_one_connection(port, worker, 15)
        }));
    }
    let acknowledged: usize = handles
        .into_iter()
        .map(|handle| handle.join().expect("worker"))
        .sum();
    assert_eq!(acknowledged, 8 * 15, "every batch was acknowledged");

    server.kill_hard();

    let total: usize = (0..8)
        .map(|worker| surviving_spans(&dir, &format!("ka{worker}")))
        .sum();
    assert_eq!(
        total,
        8 * 15 * 10,
        "every span acknowledged over a persistent connection survives SIGKILL"
    );
}

/// One GET, returning the body as text. `request_to` parses JSON, which turns
/// a Prometheus body into `Value::Null` and any assertion over it into a
/// no-op.
fn raw_get(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .expect("writes");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("reads");
    let text = String::from_utf8_lossy(&response).into_owned();
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default()
}

#[test]
fn a_commit_window_batches_more_acks_without_weakening_the_promise() {
    // The window deliberately delays each fsync so more batches ride along on
    // it. That is a latency-for-amortization trade and nothing else: an
    // acknowledgement must still mean fsynced, so SIGKILL must still lose
    // nothing. If the delay had been implemented by acknowledging early, this
    // is the test that would catch it.
    let dir = test_dir("wal-window");
    let mut server = Server::spawn_with(&dir, "wal", &["--wal-commit-window-us", "2000"]);
    let port = server.port;

    let mut handles = Vec::new();
    for worker in 0..8 {
        handles.push(std::thread::spawn(move || {
            for round in 0..10 {
                let spans: Vec<Value> = (0..10)
                    .map(|index| {
                        json!({
                            "trace_id": format!("win-{worker}"),
                            "span_id": format!("span-{round}-{index}"),
                            "name": "acknowledged", "service": "ingest",
                            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                            "attributes": {"marker": format!("win{worker}")}
                        })
                    })
                    .collect();
                let (status, _) = request_to(port, "POST", "/v1/spans", Some(&Value::Array(spans)));
                assert_eq!(status, 200);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker");
    }

    // The window should have made each fsync cover several acknowledgements.
    // Fetched as raw text: /v1/metrics is Prometheus exposition format, not
    // JSON, so the JSON helper would hand back a null and quietly assert
    // nothing.
    let text = raw_get(port, "/v1/metrics");
    let value = |name: &str| -> f64 {
        text.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(' ')?;
                (key == name).then(|| value.trim().parse::<f64>().ok())?
            })
            .unwrap_or(0.0)
    };
    let commits = value("traza_wal_commits_total");
    let fsyncs = value("traza_wal_fsync_ns_count");
    assert!(
        commits > 0.0 && fsyncs > 0.0,
        "no commits recorded:\n{text}"
    );
    assert!(
        commits / fsyncs > 1.0,
        "the window should amortize fsync across acks, got {commits} acks / {fsyncs} fsyncs"
    );

    server.kill_hard();
    let total: usize = (0..8)
        .map(|worker| surviving_spans(&dir, &format!("win{worker}")))
        .sum();
    assert_eq!(
        total, 800,
        "a delayed fsync still precedes the acknowledgement it covers"
    );
}

/// How often the hot keys are rewritten, in rounds. Often enough that many
/// merge groups carry a version of them, rare enough that resolving the key
/// does not have to walk a version in every segment in the store.
const HOT_EVERY: usize = 8;

/// Ingests batches until `until`. Each batch carries `per_batch` fresh spans
/// tagged with its own batch number, and every `HOT_EVERY` rounds it also
/// rewrites every hot key at that round's version. Returns how many batches
/// the server acknowledged.
///
/// The hot keys are the point. A primary key rewritten across segment after
/// segment gives a merge a version of it to lose, and gives recovery a way to
/// prove it did not: a stale copy that outranked a newer one comes back as a
/// version that went BACKWARDS, which no amount of counting spans would catch.
///
/// Batches are individually addressable so verification can check whole ones
/// without asking for every span at once. A query's cost grows with the
/// versions it must resolve across the segments it crosses, and this store is
/// deliberately hundreds of segments deep.
fn acknowledged_until(
    port: u16,
    worker: usize,
    per_batch: usize,
    hot_keys: usize,
    until: std::time::Instant,
) -> usize {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let mut acknowledged = 0;
    let mut leftover: Vec<u8> = Vec::new();
    for round in 0.. {
        if std::time::Instant::now() >= until {
            break;
        }
        let mut spans: Vec<Value> = (0..per_batch)
            .map(|index| {
                json!({
                    "trace_id": format!("gm-trace-{worker}"),
                    "span_id": format!("bulk-{round}-{index}"),
                    "name": "acknowledged", "service": "ingest",
                    "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                    "attributes": {"batch": format!("{worker}-{round}")}
                })
            })
            .collect();
        if round % HOT_EVERY == 0 {
            spans.extend((0..hot_keys).map(|key| {
                json!({
                    "trace_id": format!("gm-trace-{worker}"),
                    "span_id": format!("hot-{key}"),
                    "name": format!("v{round}"), "service": "ingest",
                    "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                    "attributes": {"marker": format!("hot{worker}")}
                })
            }));
        }
        let body = serde_json::to_vec(&Value::Array(spans)).expect("encodes");
        let head = format!(
            "POST /v1/spans HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&body).is_err() {
            break;
        }
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            if let Some(position) = leftover.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return acknowledged,
                Ok(read) => leftover.extend_from_slice(&chunk[..read]),
            }
        };
        let header = String::from_utf8_lossy(&leftover[..header_end]).into_owned();
        let length: usize = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);
        while leftover.len() < header_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return acknowledged,
                Ok(read) => leftover.extend_from_slice(&chunk[..read]),
            }
        }
        leftover.drain(..header_end + length);
        if header.starts_with("HTTP/1.1 200") {
            acknowledged += 1;
        }
    }
    acknowledged
}

fn matching_files(dir: &Path, predicate: impl Fn(&str) -> bool) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| predicate(name))
        .collect()
}

/// One request with a deadline, panicking rather than blocking forever.
///
/// The shared `request_to` waits indefinitely, which is right for the tests
/// that use it. Verification after a crash needs a failsafe: a hung server
/// would otherwise hang the suite. The deadline is deliberately generous and
/// is NOT the duplicate-version oracle — the assertions that follow each call
/// are (a hot key surviving other than exactly once, a stale version
/// outranking an acknowledged one, a batch coming back short). A recovered
/// store carrying superseded versions answers these queries slowly on starved
/// hardware — that cost is a tracked query-path defect, not a durability
/// failure — so a stopwatch here would convert runner weather into a verdict.
fn request_within(port: u16, target: &str, limit: Duration) -> Value {
    let (sender, receiver) = std::sync::mpsc::channel();
    let target = target.to_owned();
    let requested = target.clone();
    std::thread::spawn(move || {
        let _ = sender.send(request_to(port, "GET", &requested, None));
    });
    match receiver.recv_timeout(limit) {
        Ok((status, body)) => {
            assert_eq!(status, 200, "the recovered store answers: {body}");
            body
        }
        Err(_) => panic!(
            "{target} did not answer within {limit:?} — the recovered store is \
             hung; the deadline is a failsafe, and the recovery oracles are \
             the assertions that follow it"
        ),
    }
}

/// A compaction journal on disk means a merge is between its first output and
/// its last input — the window recovery exists for.
fn merge_in_flight(dir: &Path) -> bool {
    !matching_files(dir, |name| name.starts_with(".supersede.")).is_empty()
}

#[test]
fn a_crash_during_a_grouped_merge_recovers_exactly() {
    // A merge replaces a run of segments with a GROUP of them, and the group
    // is atomic or it is nothing. Each output holds only its own group's
    // last-write-wins view of a key while outranking every input by id, so one
    // that survives a crash its siblings did not — or that survives with no
    // journal left to account for it — shadows the newer versions the missing
    // outputs were to carry. Staged files can pose that state; only SIGKILL
    // proves the real write ordering never produces anything else.
    //
    // The kill is aimed, and three things make that possible. The maintenance
    // thread's first tick is five seconds after start. A merge declines while
    // a seal holds an unpublished id, so it needs ingest to pause to get going
    // at all — the load therefore stops before the tick. And the journal on
    // disk says a merge is in flight, so the kill waits for one to appear
    // rather than guessing at a duration that varies with the machine. Each
    // attempt then waits a little longer than the last, landing at a different
    // depth: before the first output, among them, and past them.
    const PER_BATCH: usize = 4;
    const HOT_KEYS: usize = 8;
    const WORKERS: usize = 3;
    const ATTEMPTS: usize = 3;

    let mut landed_mid_merge = 0;
    for attempt in 0..ATTEMPTS {
        let dir = test_dir(&format!("grouped-merge-crash-{attempt}"));
        // Small segments against a cap several of them wide, so every merge
        // has to split its output across a group — the shape under test.
        let mut server = Server::spawn_with(
            &dir,
            "wal",
            &[
                "--flush-spans",
                "40",
                "--compaction-fanout",
                "4",
                "--compaction-max-segment-bytes",
                "60000",
            ],
        );
        let port = server.port;
        let tick = std::time::Instant::now() + Duration::from_secs(5);

        // Stopping short of the tick is what lets compaction run at all: a
        // merge declines while a seal holds an unpublished id, so a load that
        // never pauses starves it and this would test nothing.
        let stop = tick - Duration::from_millis(600);
        let handles: Vec<_> = (0..WORKERS)
            .map(|worker| {
                std::thread::spawn(move || {
                    acknowledged_until(port, worker, PER_BATCH, HOT_KEYS, stop)
                })
            })
            .collect();
        let acknowledged: Vec<usize> = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker"))
            .collect();
        assert!(
            acknowledged.iter().all(|count| *count > HOT_EVERY),
            "every worker must have acknowledged enough to have sealed \
             repeatedly and rewritten the hot keys: {acknowledged:?}"
        );

        let deadline = tick + Duration::from_secs(30);
        let mut caught = false;
        while std::time::Instant::now() < deadline {
            if merge_in_flight(&dir) {
                caught = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if caught {
            landed_mid_merge += 1;
            // Deeper into the merge on each attempt.
            std::thread::sleep(Duration::from_millis(attempt as u64 * 60));
        }
        server.kill_hard();

        // Compaction off on the verification server: recovery is finished by
        // the time it answers, and a maintenance thread merging underneath the
        // queries only slows them down.
        let recovered = Server::spawn_with(&dir, "wal", &["--compaction-fanout", "0"]);
        for (worker, acknowledged) in acknowledged.iter().enumerate() {
            // The last acknowledged batch, and one from the middle of the run
            // that every merge has had a chance to rewrite. Whole batches, so
            // a partially recovered one is a failure and not a rounding error.
            for round in [acknowledged - 1, acknowledged / 2] {
                let body = request_within(
                    recovered.port,
                    &format!("/v1/spans?attr.batch={worker}-{round}&limit=1000"),
                    Duration::from_secs(300),
                );
                let survived = body["spans"].as_array().map(Vec::len).unwrap_or(0);
                assert_eq!(
                    survived, PER_BATCH,
                    "worker {worker}: batch {round} came back with {survived} \
                     of {PER_BATCH} spans after a SIGKILL taken mid-merge"
                );
            }

            let body = request_within(
                recovered.port,
                &format!("/v1/spans?attr.marker=hot{worker}&limit=1000"),
                Duration::from_secs(300),
            );
            let hot = body["spans"].as_array().cloned().unwrap_or_default();
            assert_eq!(
                hot.len(),
                HOT_KEYS,
                "worker {worker}: a primary key must survive exactly once, \
                 got {} for {HOT_KEYS} keys",
                hot.len()
            );
            // The newest hot version this worker had acknowledged. No key may
            // come back older than that: a lower version is a stale copy that
            // outranked a newer one — precisely what a half-published group
            // causes, and why a partial group is rolled back rather than kept.
            let floor = ((acknowledged - 1) / HOT_EVERY) * HOT_EVERY;
            for span in &hot {
                let name = span["name"].as_str().unwrap_or_default();
                let version: usize = name
                    .strip_prefix('v')
                    .and_then(|digits| digits.parse().ok())
                    .unwrap_or_else(|| panic!("worker {worker}: bad version {name:?}"));
                assert!(
                    version >= floor,
                    "worker {worker}: {} came back at v{version} after v{floor} \
                     was acknowledged — a stale copy outranked a newer one",
                    span["span_id"].as_str().unwrap_or_default()
                );
            }
        }
        let mut recovered = recovered;
        recovered.kill_hard();

        // And recovery finished the job: nothing half-written is left behind
        // to be loaded as if it were a segment.
        let leftovers = matching_files(&dir, |name| {
            name.starts_with(".supersede.") || name.ends_with(".tmp")
        });
        assert!(
            leftovers.is_empty(),
            "recovery left artifacts: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    assert!(
        landed_mid_merge > 0,
        "no SIGKILL landed inside a merge across {ATTEMPTS} attempts, so \
         nothing about grouped-merge recovery was exercised"
    );
}
