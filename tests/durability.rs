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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, durability: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_traza-server"))
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
    let count = body.as_array().map(Vec::len).unwrap_or(0);
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

/// Sends many requests down ONE connection, the way a real ingest client does
/// now that connections persist. Returns how many were acknowledged 200.
///
/// Written out rather than reusing `request_to` because the whole point is the
/// connection reuse: `request_to` opens a fresh socket per request and so
/// cannot exercise the path where a response and the next request share a
/// buffer.
fn acknowledged_over_one_connection(port: u16, worker: usize, rounds: usize) -> usize {
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
    for round in 0..rounds {
        let spans: Vec<Value> = (0..10)
            .map(|index| {
                json!({
                    "trace_id": format!("ka-trace-{worker}"),
                    "span_id": format!("span-{round}-{index}"),
                    "name": "acknowledged", "service": "ingest",
                    "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                    "attributes": {"marker": format!("ka{worker}")}
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
            if let Some(position) = leftover
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
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
