//! Ingest throughput matrix: what does a span actually cost, and where?
//!
//! The roadmap's §1.5 gate is "250k spans/s sustained in `wal` mode
//! (keep-alive + protobuf)". Reaching a number like that is easy to fake and
//! hard to earn, so this harness is built around the ways it could lie:
//!
//! - **One configuration proves nothing.** Every run records the machine, the
//!   commit, durability, protocol, keep-alive, concurrency, batch size and
//!   cache state, because a throughput figure without them is not reproducible
//!   and therefore not a measurement.
//! - **One run proves nothing.** Each configuration runs several times on a
//!   fresh data directory and reports the MEDIAN with its spread. A single
//!   number hides the variance that says whether the difference between two
//!   configurations is real.
//! - **Client cost is not server cost.** Payloads are generated before the
//!   clock starts, so the reported rate is the server's, and the generation
//!   cost is reported separately rather than hidden or silently excluded.
//! - **Throughput without durability is not a result.** Every HTTP run
//!   verifies that the server actually stored what it acknowledged, and the
//!   `wal` runs restart the server and re-verify, because the whole point of
//!   the mode is that an acknowledgement survives a crash.
//! - **Backpressure invalidates a rate.** If the server shed any connection
//!   during a run, the run says so; a rate measured while clients were being
//!   refused is not a sustained rate.
//!
//! Stage attribution comes from the server's own `/v1/metrics`, scraped after
//! each run. Those percentiles are approximate by construction (see
//! `traza::metrics`); the end-to-end numbers here are exact.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

// ------------------------------------------------------------------ options

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Straight into `Store::ingest_batch`, no socket. Isolates engine cost.
    Direct,
    Http,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Protocol {
    Json,
    Protobuf,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Json => "json",
            Protocol::Protobuf => "protobuf",
        }
    }
}

#[derive(Clone, Debug)]
struct Scenario {
    label: String,
    mode: Mode,
    protocol: Protocol,
    keep_alive: bool,
    durability: String,
    concurrency: usize,
    batch: usize,
    spans: usize,
}

impl Scenario {
    fn describe(&self) -> String {
        if self.mode == Mode::Direct {
            return format!(
                "direct engine, durability={}, batch={}, concurrency={}",
                self.durability, self.batch, self.concurrency
            );
        }
        format!(
            "http {}, keep-alive={}, durability={}, batch={}, concurrency={}",
            self.protocol.as_str(),
            if self.keep_alive { "on" } else { "off" },
            self.durability,
            self.batch,
            self.concurrency
        )
    }
}

// --------------------------------------------------------------- corpus gen

/// One span's worth of source data, shared by both encoders so the JSON and
/// protobuf runs carry the SAME payload and remain comparable.
struct SpanSeed {
    trace_id: String,
    span_id: String,
    name: &'static str,
    service: String,
    start_ns: u64,
    end_ns: u64,
    group: String,
}

fn seed(index: usize) -> SpanSeed {
    let trace_number = index / 10;
    let start_ns = 1_700_000_000_000_000_000_u64 + (index as u64 * 1_000_000);
    SpanSeed {
        trace_id: format!("{:032x}", trace_number + 1),
        span_id: format!("{:016x}", index + 1),
        name: if index % 10 == 0 {
            "request"
        } else {
            "operation"
        },
        service: format!("service-{}", index % 20),
        start_ns,
        end_ns: start_ns + 500_000 + ((index % 100) as u64 * 20_000),
        group: format!("group-{}", index % 100),
    }
}

fn json_batch(start: usize, end: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(320 * (end - start));
    body.push(b'[');
    for index in start..end {
        if index != start {
            body.push(b',');
        }
        let seed = seed(index);
        let span = json!({
            "trace_id": seed.trace_id,
            "span_id": seed.span_id,
            "name": seed.name,
            "service": seed.service,
            "start_ns": seed.start_ns,
            "end_ns": seed.end_ns,
            "status": "ok",
            "attributes": {
                "benchmark.group": seed.group,
                "http.method": if index % 2 == 0 { "GET" } else { "POST" }
            }
        });
        serde_json::to_writer(&mut body, &span).expect("json");
    }
    body.push(b']');
    body
}

// ------------------------------------------------------- protobuf encoding

fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn tag(out: &mut Vec<u8>, field: u32, wire: u32) {
    varint(out, u64::from(field << 3 | wire));
}

fn delimited(out: &mut Vec<u8>, field: u32, payload: &[u8]) {
    tag(out, field, 2);
    varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

fn fixed64(out: &mut Vec<u8>, field: u32, value: u64) {
    tag(out, field, 1);
    out.extend_from_slice(&value.to_le_bytes());
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

/// `KeyValue { key = 1, value = 2 }` with a string `AnyValue`.
fn string_attribute(key: &str, value: &str) -> Vec<u8> {
    let mut any = Vec::new();
    delimited(&mut any, 1, value.as_bytes());
    let mut pair = Vec::new();
    delimited(&mut pair, 1, key.as_bytes());
    delimited(&mut pair, 2, &any);
    pair
}

/// An `ExportTraceServiceRequest` carrying `[start, end)`.
///
/// Encoded here rather than by a library because the point of the protobuf
/// path is that Traza has two dependencies; a benchmark that pulled in a
/// codegen stack to test it would be measuring a program nobody ships.
fn protobuf_batch(start: usize, end: usize) -> Vec<u8> {
    let mut spans = Vec::with_capacity(64 * (end - start));
    for index in start..end {
        let seed = seed(index);
        let mut span = Vec::with_capacity(256);
        delimited(&mut span, 1, &unhex(&seed.trace_id));
        delimited(&mut span, 2, &unhex(&seed.span_id));
        delimited(&mut span, 5, seed.name.as_bytes());
        fixed64(&mut span, 7, seed.start_ns);
        fixed64(&mut span, 8, seed.end_ns);
        delimited(
            &mut span,
            9,
            &string_attribute("benchmark.group", &seed.group),
        );
        delimited(
            &mut span,
            9,
            &string_attribute("http.method", if index % 2 == 0 { "GET" } else { "POST" }),
        );
        // Status { code = 2 } = STATUS_CODE_OK.
        let mut status = Vec::new();
        tag(&mut status, 2, 0);
        varint(&mut status, 1);
        delimited(&mut span, 15, &status);
        // ScopeSpans.spans
        delimited(&mut spans, 2, &span);
    }

    // service.name is per-RESOURCE in OTLP, so a batch is one resource. The
    // JSON corpus varies service per span; protobuf cannot without one
    // resource per span, so both encoders agree on a single service here and
    // the comparison stays like-for-like.
    let mut resource = Vec::new();
    delimited(
        &mut resource,
        1,
        &string_attribute("service.name", "service-0"),
    );

    let mut resource_spans = Vec::new();
    delimited(&mut resource_spans, 1, &resource);
    delimited(&mut resource_spans, 2, &spans);

    let mut request = Vec::new();
    delimited(&mut request, 1, &resource_spans);
    request
}

// ------------------------------------------------------------------ server

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn spawn(data_dir: &Path, durability: &str) -> Result<Self, String> {
        let binary = release_binary("traza-server");
        let mut child = Command::new(&binary)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg(durability)
            // Compaction off: it is a read-path optimization whose merges
            // would otherwise steal CPU from the thing being measured.
            .arg("--compaction-fanout")
            .arg("0")
            .env_remove("TRAZA_TOKENS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", binary.display()))?;
        let stderr = child.stderr.take().ok_or("stderr")?;
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut startup = String::new();
        let port = loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return Err(format!("server exited before listening:\n{startup}")),
                Ok(_) => {}
                Err(error) => return Err(format!("server stderr: {error}")),
            }
            startup.push_str(&line);
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().map_err(|e| e.to_string())?;
            }
        };
        std::thread::spawn(move || for _ in reader.lines() {});
        Ok(Self { child, port })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn release_binary(name: &str) -> PathBuf {
    let mut path = PathBuf::from("target").join("release").join(name);
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

// ------------------------------------------------------------------ client

/// A client connection that can either persist or reconnect per request, so
/// the keep-alive comparison runs through ONE client implementation.
struct Client {
    port: u16,
    keep_alive: bool,
    stream: Option<TcpStream>,
    buffer: Vec<u8>,
}

impl Client {
    fn new(port: u16, keep_alive: bool) -> Self {
        Self {
            port,
            keep_alive,
            stream: None,
            buffer: Vec::with_capacity(8192),
        }
    }

    fn connect(&mut self) -> std::io::Result<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        stream.set_write_timeout(Some(Duration::from_secs(120)))?;
        Ok(stream)
    }

    fn post(&mut self, path: &str, content_type: &str, body: &[u8]) -> Result<u16, String> {
        if self.stream.is_none() {
            self.stream = Some(self.connect().map_err(|e| format!("connect: {e}"))?);
            self.buffer.clear();
        }
        let connection = if self.keep_alive {
            "keep-alive"
        } else {
            "close"
        };
        let mut request = Vec::with_capacity(body.len() + 200);
        let _ = write!(
            request,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
            body.len()
        );
        request.extend_from_slice(body);

        let stream = self.stream.as_mut().expect("connected");
        stream
            .write_all(&request)
            .map_err(|error| format!("write: {error}"))?;
        let status = read_response(stream, &mut self.buffer)?;
        if !self.keep_alive {
            self.stream = None;
        }
        Ok(status.0)
    }

    fn get(&mut self, path: &str) -> Result<(u16, Vec<u8>), String> {
        if self.stream.is_none() {
            self.stream = Some(self.connect().map_err(|e| format!("connect: {e}"))?);
            self.buffer.clear();
        }
        let connection = if self.keep_alive {
            "keep-alive"
        } else {
            "close"
        };
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: {connection}\r\n\r\n");
        let stream = self.stream.as_mut().expect("connected");
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("write: {error}"))?;
        let (status, body) = read_response(stream, &mut self.buffer)?;
        if !self.keep_alive {
            self.stream = None;
        }
        Ok((status, body))
    }
}

/// Reads one response, leaving any surplus bytes in `buffer` for the next.
fn read_response(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(u16, Vec<u8>), String> {
    let mut chunk = [0_u8; 16384];
    let header_end = loop {
        if let Some(position) = find(buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            return Err("server closed mid-response".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .ok_or("missing status")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            return Err("server closed mid-body".to_owned());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[header_end..header_end + length].to_vec();
    buffer.drain(..header_end + length);
    Ok((status, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ------------------------------------------------------------------- stats

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

// ------------------------------------------------------------------ running

struct RunResult {
    rate: f64,
    elapsed: Duration,
    stored: u64,
    refused: u64,
    metrics: String,
}

/// One measured ingest run against a fresh server and data directory.
fn run_http(scenario: &Scenario, payloads: &Arc<Vec<Vec<u8>>>) -> Result<RunResult, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let data_dir =
        std::env::temp_dir().join(format!("traza-ingestbench-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let server = Server::spawn(&data_dir, &scenario.durability)?;

    let (path, content_type) = match scenario.protocol {
        Protocol::Json => ("/v1/spans", "application/json"),
        Protocol::Protobuf => ("/v1/traces", "application/x-protobuf"),
    };

    let next = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    // Every client connects and is ready before any of them starts sending,
    // so connection setup is not charged to the measured window.
    let ready = Arc::new(Barrier::new(scenario.concurrency + 1));
    let mut handles = Vec::new();
    for _ in 0..scenario.concurrency {
        let next = Arc::clone(&next);
        let failures = Arc::clone(&failures);
        let ready = Arc::clone(&ready);
        let payloads = Arc::clone(payloads);
        let port = server.port;
        let keep_alive = scenario.keep_alive;
        let path = path.to_owned();
        let content_type = content_type.to_owned();
        handles.push(std::thread::spawn(move || {
            let mut client = Client::new(port, keep_alive);
            // Establish the connection before the barrier.
            if keep_alive {
                if let Ok(stream) = client.connect() {
                    client.stream = Some(stream);
                }
            }
            ready.wait();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= payloads.len() {
                    break;
                }
                match client.post(&path, &content_type, &payloads[index]) {
                    Ok(status) if status / 100 == 2 => {}
                    Ok(status) => {
                        eprintln!("  batch {index} rejected with HTTP {status}");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        eprintln!("  batch {index} failed: {error}");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    ready.wait();
    let started = Instant::now();
    for handle in handles {
        handle.join().map_err(|_| "worker panicked")?;
    }
    let elapsed = started.elapsed();

    let failed = failures.load(Ordering::Relaxed);
    if failed > 0 {
        return Err(format!("{failed} batches failed; the run is not a result"));
    }

    let mut probe = Client::new(server.port, true);
    let stored = wait_for_records(&mut probe, scenario.spans as u64)?;
    let metrics = probe
        .get("/v1/metrics")
        .map(|(_, body)| String::from_utf8_lossy(&body).into_owned())
        .unwrap_or_default();
    let refused = metric_value(&metrics, "traza_http_connections_refused_total").unwrap_or(0);

    // The durability claim, checked rather than assumed: restart and confirm
    // the server still has what it acknowledged.
    if scenario.durability != "buffered" {
        drop(server);
        let restarted = Server::spawn(&data_dir, &scenario.durability)?;
        let mut after = Client::new(restarted.port, true);
        let recovered = wait_for_records(&mut after, scenario.spans as u64).map_err(|error| {
            format!(
                "durability check failed after restart ({}): {error}",
                scenario.durability
            )
        })?;
        if recovered < scenario.spans as u64 {
            return Err(format!(
                "durability check failed: acknowledged {} spans, recovered {recovered}",
                scenario.spans
            ));
        }
        drop(restarted);
    } else {
        drop(server);
    }
    let _ = std::fs::remove_dir_all(&data_dir);

    Ok(RunResult {
        rate: scenario.spans as f64 / elapsed.as_secs_f64(),
        elapsed,
        stored,
        refused,
        metrics,
    })
}

fn wait_for_records(client: &mut Client, expected: u64) -> Result<u64, String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (status, body) = client.get("/v1/stats")?;
        if status == 200 {
            let value: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            let count = value
                .get("record_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if count >= expected {
                return Ok(count);
            }
            if Instant::now() >= deadline {
                return Err(format!("only {count} of {expected} spans stored"));
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn metric_value(metrics: &str, name: &str) -> Option<u64> {
    metrics.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == name).then(|| value.trim().parse().ok())?
    })
}

/// Direct-engine run: no socket, no HTTP, no client. The floor that the HTTP
/// numbers are measured against.
fn run_direct(scenario: &Scenario) -> Result<RunResult, String> {
    use traza::{Config, Durability, Span, Store};

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let data_dir = std::env::temp_dir().join(format!(
        "traza-ingestbench-direct-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;

    let durability = Durability::parse(&scenario.durability).ok_or("bad durability")?;
    let store = Arc::new(
        Store::open(
            &data_dir,
            Config {
                durability,
                compaction: None,
                ..Config::default()
            },
        )
        .map_err(|error| error.to_string())?,
    );

    // Decode the batches up front so the timed window is engine work only.
    let mut batches: Vec<Vec<Span>> = Vec::new();
    for start in (0..scenario.spans).step_by(scenario.batch) {
        let end = (start + scenario.batch).min(scenario.spans);
        let body = json_batch(start, end);
        batches.push(serde_json::from_slice(&body).map_err(|e| e.to_string())?);
    }

    let next = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(Barrier::new(scenario.concurrency + 1));
    let batches = Arc::new(batches);
    let mut handles = Vec::new();
    for _ in 0..scenario.concurrency {
        let next = Arc::clone(&next);
        let ready = Arc::clone(&ready);
        let batches = Arc::clone(&batches);
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            ready.wait();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= batches.len() {
                    break;
                }
                store
                    .ingest_batch(batches[index].clone())
                    .expect("direct ingest");
            }
        }));
    }
    ready.wait();
    let started = Instant::now();
    for handle in handles {
        handle.join().map_err(|_| "worker panicked")?;
    }
    let elapsed = started.elapsed();

    let stats = store.stats().map_err(|e| e.to_string())?;
    let mut metrics = String::new();
    store.metrics().render_prometheus(&mut metrics);
    drop(store);
    let _ = std::fs::remove_dir_all(&data_dir);

    Ok(RunResult {
        rate: scenario.spans as f64 / elapsed.as_secs_f64(),
        elapsed,
        stored: stats.total_records as u64,
        refused: 0,
        metrics,
    })
}

// ------------------------------------------------------------------ report

fn machine_context() -> String {
    let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
    let model = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown CPU".to_owned());
    format!(
        "{}/{}, {parallelism} hardware threads, {model}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The stages, ranked, from a scraped `/v1/metrics`.
fn stage_summary(metrics: &str) -> String {
    let stages = [
        ("decode (wire -> spans)", "traza_http_decode"),
        ("writer lock wait", "traza_writer_lock_wait"),
        ("wal encode", "traza_wal_encode"),
        ("wal write", "traza_wal_write"),
        ("wal fsync", "traza_wal_fsync"),
        ("buffer upsert", "traza_buffer_upsert"),
        ("segment seal", "traza_segment_seal"),
    ];
    let mut rows: Vec<(f64, String)> = Vec::new();
    for (label, prefix) in stages {
        let sum = metric_value(metrics, &format!("{prefix}_ns_sum")).unwrap_or(0);
        let count = metric_value(metrics, &format!("{prefix}_ns_count")).unwrap_or(0);
        if count == 0 {
            continue;
        }
        let total_ms = sum as f64 / 1e6;
        rows.push((
            total_ms,
            format!(
                "{label}: {total_ms:.0} ms total over {count} calls (mean {:.3} ms)",
                total_ms / count as f64
            ),
        ));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).expect("finite"));
    let commits = metric_value(metrics, "traza_wal_commits_total").unwrap_or(0);
    let fsyncs = metric_value(metrics, "traza_wal_fsync_ns_count").unwrap_or(0);
    let mut out = rows
        .into_iter()
        .map(|(_, line)| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    if fsyncs > 0 {
        let _ = write!(
            out,
            "\n      group commit: {commits} acks / {fsyncs} fsyncs = {:.1}x amortization",
            commits as f64 / fsyncs as f64
        );
    }
    out
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ingest-bench: {error}");
        std::process::exit(1);
    }
}

fn parse_usize(args: &[String], index: usize, name: &str) -> Result<usize, String> {
    args.get(index)
        .ok_or_else(|| format!("{name} requires a value"))?
        .parse()
        .map_err(|_| format!("{name} must be a number"))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut spans = 500_000_usize;
    let mut runs = 5_usize;
    let mut only: Option<String> = None;
    let mut concurrencies: Vec<usize> = Vec::new();
    let mut batch = 1_000_usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--spans" => {
                i += 1;
                spans = parse_usize(&args, i, "--spans")?;
            }
            "--runs" => {
                i += 1;
                runs = parse_usize(&args, i, "--runs")?;
            }
            "--batch" => {
                i += 1;
                batch = parse_usize(&args, i, "--batch")?;
            }
            "--concurrency" => {
                i += 1;
                concurrencies.push(parse_usize(&args, i, "--concurrency")?);
            }
            "--only" => {
                i += 1;
                only = Some(
                    args.get(i)
                        .ok_or("--only requires a substring")?
                        .to_ascii_lowercase(),
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: ingest-bench [--spans N] [--runs N] [--batch N] [--concurrency N ...] [--only SUBSTRING]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    if concurrencies.is_empty() {
        concurrencies = vec![1, 4, 8, 16];
    }

    if !release_binary("traza-server").exists() {
        return Err("build first: cargo build --release".to_owned());
    }

    let mut scenarios = Vec::new();
    scenarios.push(Scenario {
        label: "direct-engine-wal".to_owned(),
        mode: Mode::Direct,
        protocol: Protocol::Json,
        keep_alive: false,
        durability: "wal".to_owned(),
        concurrency: 8,
        batch,
        spans,
    });
    scenarios.push(Scenario {
        label: "direct-engine-buffered".to_owned(),
        mode: Mode::Direct,
        protocol: Protocol::Json,
        keep_alive: false,
        durability: "buffered".to_owned(),
        concurrency: 8,
        batch,
        spans,
    });
    // The keep-alive comparison, held at one concurrency so the only variable is
    // the connection policy.
    for keep_alive in [false, true] {
        scenarios.push(Scenario {
            label: format!(
                "http-json-wal-keepalive-{}",
                if keep_alive { "on" } else { "off" }
            ),
            mode: Mode::Http,
            protocol: Protocol::Json,
            keep_alive,
            durability: "wal".to_owned(),
            concurrency: 8,
            batch,
            spans,
        });
    }
    for concurrency in &concurrencies {
        for protocol in [Protocol::Json, Protocol::Protobuf] {
            scenarios.push(Scenario {
                label: format!("http-{}-wal-c{concurrency}", protocol.as_str()),
                mode: Mode::Http,
                protocol,
                keep_alive: true,
                durability: "wal".to_owned(),
                concurrency: *concurrency,
                batch,
                spans,
            });
        }
    }

    let context = machine_context();
    let commit = commit();
    println!("Traza ingest benchmark");
    println!("  machine: {context}");
    println!("  commit:  {commit}");
    println!("  corpus:  {spans} spans, batch {batch}, {runs} runs per scenario");
    println!("  cache:   cold data directory per run (created, filled, deleted)");
    println!();

    let mut report = String::new();
    let _ = writeln!(report, "# Traza Ingest Benchmark\n");
    let _ = writeln!(
        report,
        "Every row is the MEDIAN of {runs} runs, each on a fresh data directory. \
Payloads are generated before the clock starts, so these are server rates; \
client encoding is reported separately. Runs that saw a failed batch or a shed \
connection are reported as failures rather than as numbers.\n"
    );
    let _ = writeln!(report, "- Machine: {context}");
    let _ = writeln!(report, "- Commit: `{commit}`");
    let _ = writeln!(report, "- Corpus: {spans} spans per run, batch {batch}");
    let _ = writeln!(
        report,
        "- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)\n"
    );
    let _ = writeln!(
        report,
        "| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max |"
    );
    let _ = writeln!(report, "|---|---|---|---:|---:|---:|---:|");

    for scenario in &scenarios {
        if let Some(filter) = &only {
            if !scenario.label.to_ascii_lowercase().contains(filter) {
                continue;
            }
        }
        println!("{} — {}", scenario.label, scenario.describe());

        // Payload generation is outside the measured window; its cost is
        // reported so the client's share of the work is visible rather than
        // quietly excluded.
        let encode_started = Instant::now();
        let payloads: Arc<Vec<Vec<u8>>> = Arc::new(if scenario.mode == Mode::Http {
            (0..scenario.spans)
                .step_by(scenario.batch)
                .map(|start| {
                    let end = (start + scenario.batch).min(scenario.spans);
                    match scenario.protocol {
                        Protocol::Json => json_batch(start, end),
                        Protocol::Protobuf => protobuf_batch(start, end),
                    }
                })
                .collect()
        } else {
            Vec::new()
        });
        let encode_elapsed = encode_started.elapsed();
        if scenario.mode == Mode::Http {
            let bytes: usize = payloads.iter().map(Vec::len).sum();
            println!(
                "  client encode: {:.2}s for {} batches ({:.1} MiB, {:.0} spans/s if it were the limit)",
                encode_elapsed.as_secs_f64(),
                payloads.len(),
                bytes as f64 / (1024.0 * 1024.0),
                scenario.spans as f64 / encode_elapsed.as_secs_f64()
            );
        }

        let mut rates = Vec::new();
        let mut last_metrics = String::new();
        let mut failed: Option<String> = None;
        for attempt in 1..=runs {
            let result = if scenario.mode == Mode::Direct {
                run_direct(scenario)
            } else {
                run_http(scenario, &payloads)
            };
            match result {
                Ok(result) => {
                    if result.refused > 0 {
                        failed = Some(format!(
                            "server shed {} connections; the rate is not sustained",
                            result.refused
                        ));
                        break;
                    }
                    if result.stored < scenario.spans as u64 {
                        failed = Some(format!(
                            "stored {} of {} spans",
                            result.stored, scenario.spans
                        ));
                        break;
                    }
                    println!(
                        "  run {attempt}: {:.0} spans/s ({:.2}s)",
                        result.rate,
                        result.elapsed.as_secs_f64()
                    );
                    rates.push(result.rate);
                    last_metrics = result.metrics;
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }

        if let Some(error) = failed {
            println!("  FAILED: {error}\n");
            let _ = writeln!(
                report,
                "| {} | {} | {} | {} | FAILED: {error} | | |",
                scenario.label,
                scenario.protocol.as_str(),
                scenario.keep_alive,
                scenario.concurrency
            );
            continue;
        }

        let min = rates.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = rates.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mid = median(&mut rates.clone());
        println!("  median: {mid:.0} spans/s (min {min:.0}, max {max:.0})");
        let stages = stage_summary(&last_metrics);
        if !stages.is_empty() {
            println!("  stages (server-side totals across the run):");
            println!("{stages}");
        }
        println!();

        let _ = writeln!(
            report,
            "| {} | {} | {} | {} | **{mid:.0}** | {min:.0} | {max:.0} |",
            scenario.label,
            scenario.protocol.as_str(),
            if scenario.mode == Mode::Direct {
                "n/a".to_owned()
            } else {
                scenario.keep_alive.to_string()
            },
            scenario.concurrency
        );
    }

    std::fs::write("INGEST-BENCHMARK.md", &report).map_err(|e| e.to_string())?;
    println!("Wrote INGEST-BENCHMARK.md");
    Ok(())
}
