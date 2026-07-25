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

/// The server's `--profile` values, in the order the report should show them.
const PROFILES: [&str; 3] = ["throughput", "balanced", "latency"];

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
    /// Extra server flags, e.g. `--profile throughput`. Empty means the
    /// server's own defaults.
    server_args: Vec<String>,
    /// Offered load in spans/s, or `None` for closed-loop (send as fast as
    /// the server will take it).
    ///
    /// Closed-loop latency is not an independent measurement: with a fixed
    /// number of saturating workers, Little's law pins it to
    /// concurrency/throughput, so anything that raises throughput "improves"
    /// latency and a latency-tuned configuration cannot be distinguished from
    /// a fast one. Holding the ARRIVAL RATE fixed instead is what makes the
    /// two separable.
    offered_rate: Option<f64>,
}

impl Scenario {
    fn describe(&self) -> String {
        let extra = if self.server_args.is_empty() {
            String::new()
        } else {
            format!(", server {}", self.server_args.join(" "))
        };
        if self.mode == Mode::Direct {
            return format!(
                "direct engine, durability={}, batch={}, concurrency={}{extra}",
                self.durability, self.batch, self.concurrency
            );
        }
        let load = match self.offered_rate {
            Some(rate) => format!(", offered {rate:.0} spans/s (open loop)"),
            None => ", saturating (closed loop)".to_owned(),
        };
        format!(
            "http {}, keep-alive={}, durability={}, batch={}, concurrency={}{load}{extra}",
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
    fn spawn(data_dir: &Path, durability: &str, extra: &[String]) -> Result<Self, String> {
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
            // would otherwise steal CPU from the thing being measured. No
            // profile changes it, so holding it off keeps a profile
            // comparison to the knobs the profile actually sets.
            .arg("--compaction-fanout")
            .arg("0")
            .args(extra)
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
    // TRAZA_BENCH_SERVER points the harness at a different build of the
    // server — an older commit, say — so a before/after comparison runs
    // through ONE client. Measuring the two with different clients would put
    // the client's own change inside the difference being attributed to the
    // server.
    if name == "traza-server" {
        if let Ok(path) = std::env::var("TRAZA_BENCH_SERVER") {
            return PathBuf::from(path);
        }
    }
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
        let (status, _, closing) = read_response(stream, &mut self.buffer)?;
        if !self.keep_alive || closing {
            self.stream = None;
            self.buffer.clear();
        }
        Ok(status)
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
        let (status, body, closing) = read_response(stream, &mut self.buffer)?;
        if !self.keep_alive || closing {
            self.stream = None;
            self.buffer.clear();
        }
        Ok((status, body))
    }
}

/// Reads one response, leaving any surplus bytes in `buffer` for the next.
fn read_response(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<(u16, Vec<u8>, bool), String> {
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
    // A server may close regardless of what the client asked for — the
    // pre-keep-alive server always did. Honouring that is what lets this one
    // harness measure both, so the comparison is not confounded by using a
    // different client for each.
    let closing = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("connection")
                && value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close"))
        })
    });
    Ok((status, body, closing))
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

/// Per-request latencies for one run, in milliseconds.
///
/// Exact samples, not a bucketed histogram: a run holds one `u64` per request,
/// which even at batch=20 is a few tens of thousands of values. The server's
/// own `/v1/metrics` percentiles are approximate by construction; these are
/// not, and the two are reported separately rather than blended.
#[derive(Clone, Debug, Default)]
struct Latencies {
    /// Sorted ascending on construction.
    nanos: Vec<u64>,
}

impl Latencies {
    fn new(mut nanos: Vec<u64>) -> Self {
        nanos.sort_unstable();
        Self { nanos }
    }

    /// Nearest-rank percentile, in milliseconds.
    fn percentile(&self, quantile: f64) -> f64 {
        if self.nanos.is_empty() {
            return f64::NAN;
        }
        let rank = (quantile * self.nanos.len() as f64).ceil() as usize;
        let index = rank.saturating_sub(1).min(self.nanos.len() - 1);
        self.nanos[index] as f64 / 1e6
    }

    fn p50(&self) -> f64 {
        self.percentile(0.50)
    }

    fn p95(&self) -> f64 {
        self.percentile(0.95)
    }

    fn p99(&self) -> f64 {
        self.percentile(0.99)
    }

    fn max_ms(&self) -> f64 {
        self.nanos.last().map_or(f64::NAN, |ns| *ns as f64 / 1e6)
    }
}

// ------------------------------------------------------------------ running

struct RunResult {
    rate: f64,
    elapsed: Duration,
    stored: u64,
    refused: u64,
    metrics: String,
    /// One sample per request (per `ingest_batch` call in direct mode).
    latency: Latencies,
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
    let server = Server::spawn(&data_dir, &scenario.durability, &scenario.server_args)?;

    let (path, content_type) = match scenario.protocol {
        Protocol::Json => ("/v1/spans", "application/json"),
        Protocol::Protobuf => ("/v1/traces", "application/x-protobuf"),
    };

    let next = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    // Worst observed gap between when a batch was DUE and when it actually
    // went out. In an open-loop run this is the honest "did the offered rate
    // exceed capacity" signal; see the check after the join.
    let worst_lag_ns = Arc::new(AtomicUsize::new(0));
    // Seconds between successive batch departures for the run as a whole.
    let interval = scenario
        .offered_rate
        .map(|rate| scenario.batch as f64 / rate);
    // Every client connects and is ready before any of them starts sending,
    // so connection setup is not charged to the measured window.
    let ready = Arc::new(Barrier::new(scenario.concurrency + 1));
    // A second rendezvous so the schedule origin is published before any
    // worker can read it: `ready` releases every thread at once, which left
    // the workers racing the main thread's write.
    let go = Arc::new(Barrier::new(scenario.concurrency + 1));
    // Established once, between the two barriers, so all workers share one
    // schedule origin and it is exactly the run's start instant.
    let origin = Arc::new(std::sync::OnceLock::<Instant>::new());
    let mut handles = Vec::new();
    for _ in 0..scenario.concurrency {
        let next = Arc::clone(&next);
        let failures = Arc::clone(&failures);
        let ready = Arc::clone(&ready);
        let go = Arc::clone(&go);
        let payloads = Arc::clone(payloads);
        let worst_lag_ns = Arc::clone(&worst_lag_ns);
        let origin = Arc::clone(&origin);
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
            go.wait();
            let start = *origin
                .get()
                .expect("origin published before the go barrier");
            let mut samples = Vec::new();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= payloads.len() {
                    break;
                }
                // Open loop: batch `index` is DUE at a fixed offset from the
                // run's origin, whatever the server is doing. Latency is
                // measured from that due time rather than from the actual
                // send, which is what keeps a backed-up server from hiding
                // behind coordinated omission — if the client is late
                // because the previous request was slow, the lateness belongs
                // in the sample.
                let due =
                    interval.map(|seconds| start + Duration::from_secs_f64(seconds * index as f64));
                if let Some(due) = due {
                    let now = Instant::now();
                    if now < due {
                        std::thread::sleep(due - now);
                    } else {
                        let lag = (now - due).as_nanos() as usize;
                        worst_lag_ns.fetch_max(lag, Ordering::Relaxed);
                    }
                }
                // Closed loop: the client-observed latency of one
                // acknowledged batch, request written to response read. In
                // keep-alive-off runs it includes the reconnect, because that
                // is what the mode costs.
                let sent = Instant::now();
                let outcome = client.post(&path, &content_type, &payloads[index]);
                let done = Instant::now();
                let observed = match due {
                    Some(due) => done.saturating_duration_since(due),
                    None => done - sent,
                };
                match outcome {
                    Ok(status) if status / 100 == 2 => samples.push(observed.as_nanos() as u64),
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
            samples
        }));
    }

    ready.wait();
    let started = Instant::now();
    let _ = origin.set(started);
    go.wait();
    let mut samples = Vec::new();
    for handle in handles {
        samples.extend(handle.join().map_err(|_| "worker panicked")?);
    }
    let elapsed = started.elapsed();

    // An open-loop run that could not sustain the offered rate did not measure
    // the offered rate; it measured capacity, and its latencies describe a
    // saturated queue rather than a configuration. Reject it rather than
    // publish it, on the same principle as a shed connection.
    //
    // The test is DELIVERED RATE, not worst lateness. A single deep stall — a
    // segment seal, say — makes the next several batches late without the run
    // failing to deliver the load, and that lateness is precisely what the
    // latency percentiles exist to report. Gating on worst lag would throw
    // away the measurement for exhibiting the effect being measured; only
    // lateness the run never recovers from shows up as a rate shortfall.
    if let Some(offered) = scenario.offered_rate {
        let achieved = scenario.spans as f64 / elapsed.as_secs_f64();
        if achieved < offered * 0.95 {
            return Err(format!(
                "offered rate exceeded capacity: delivered {achieved:.0} of {offered:.0} spans/s \
(worst lateness {:.1} ms), so this is a saturation measurement, not an open-loop one",
                worst_lag_ns.load(Ordering::Relaxed) as f64 / 1e6
            ));
        }
    }

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
        let restarted = Server::spawn(&data_dir, &scenario.durability, &scenario.server_args)?;
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
        latency: Latencies::new(samples),
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
    // Each batch is HANDED OVER rather than cloned. Cloning inside the timed
    // loop charged the engine for a thousand-span deep copy per batch, which
    // is client work: it made the direct-engine floor look slower than the
    // HTTP path it is supposed to bound.
    let batches: Arc<Vec<std::sync::Mutex<Option<Vec<Span>>>>> = Arc::new(
        batches
            .into_iter()
            .map(|batch| std::sync::Mutex::new(Some(batch)))
            .collect(),
    );
    let mut handles = Vec::new();
    for _ in 0..scenario.concurrency {
        let next = Arc::clone(&next);
        let ready = Arc::clone(&ready);
        let batches = Arc::clone(&batches);
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            ready.wait();
            let mut samples = Vec::new();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= batches.len() {
                    break;
                }
                // Indices are handed out atomically, so exactly one worker
                // ever takes a given batch.
                let batch = batches[index].lock().expect("batch").take();
                if let Some(batch) = batch {
                    // Only the engine call is timed; taking the batch out of
                    // the handoff slot is harness bookkeeping.
                    let started = Instant::now();
                    store.ingest_batch(batch).expect("direct ingest");
                    samples.push(started.elapsed().as_nanos() as u64);
                }
            }
            samples
        }));
    }
    ready.wait();
    let started = Instant::now();
    let mut samples = Vec::new();
    for handle in handles {
        samples.extend(handle.join().map_err(|_| "worker panicked")?);
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
        latency: Latencies::new(samples),
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

/// A compact scenario label for a variant's flags: `--flush-spans 3000`
/// becomes `flush-spans-3000`.
fn variant_label(flags: &[String]) -> String {
    flags
        .iter()
        .map(|flag| flag.trim_start_matches("--"))
        .collect::<Vec<_>>()
        .join("-")
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
    // Extra server flags applied to every HTTP scenario, so a configuration
    // can be swept without editing and rebuilding this file. This is how the
    // profile constants were chosen rather than guessed.
    let mut server_args: Vec<String> = Vec::new();
    // One extra scenario per variant, so a parameter sweep is INTERLEAVED with
    // everything else in a single run. Sweeping by re-invoking the harness
    // once per value makes each value a separate block of wall-clock time,
    // which is exactly the bias round-robin scheduling exists to remove.
    let mut variants: Vec<Vec<String>> = Vec::new();
    let mut offered_rate: Option<f64> = None;
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
            "--server-arg" => {
                i += 1;
                let raw = args.get(i).ok_or("--server-arg requires a value")?;
                server_args.extend(raw.split_whitespace().map(str::to_owned));
            }
            "--variant" => {
                i += 1;
                let raw = args.get(i).ok_or("--variant requires a value")?;
                let flags: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
                if flags.is_empty() {
                    return Err("--variant requires at least one flag".to_owned());
                }
                variants.push(flags);
            }
            "--offered-rate" => {
                i += 1;
                offered_rate = Some(
                    args.get(i)
                        .ok_or("--offered-rate requires a value")?
                        .parse()
                        .map_err(|_| "--offered-rate must be spans/s".to_owned())?,
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: ingest-bench [--spans N] [--runs N] [--batch N] [--concurrency N ...] \
[--only SUBSTRING] [--server-arg \"--flag value\" ...] \
[--variant \"--flag value\" ... (one extra interleaved scenario per variant)] \
[--offered-rate SPANS_PER_SEC (adds open-loop rows at a fixed arrival rate)]"
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
    for durability in ["wal", "buffered"] {
        scenarios.push(Scenario {
            label: format!("direct-engine-{durability}"),
            mode: Mode::Direct,
            protocol: Protocol::Json,
            keep_alive: false,
            durability: durability.to_owned(),
            concurrency: 8,
            batch,
            spans,
            server_args: Vec::new(),
            offered_rate: None,
        });
    }
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
            server_args: Vec::new(),
            offered_rate: None,
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
                server_args: Vec::new(),
                offered_rate: None,
            });
        }
    }
    // The profile comparison. Every other variable is pinned — same protocol,
    // same keep-alive, same durability, same corpus — so the only thing that
    // differs between the rows at a given concurrency is `--profile`.
    // Concurrency is swept because the profiles' whole disagreement is about
    // what happens when there is, or is not, other work to batch with.
    for profile in PROFILES {
        for concurrency in &concurrencies {
            scenarios.push(Scenario {
                label: format!("profile-{profile}-c{concurrency}"),
                mode: Mode::Http,
                protocol: Protocol::Json,
                keep_alive: true,
                durability: "wal".to_owned(),
                concurrency: *concurrency,
                batch,
                spans,
                server_args: vec!["--profile".to_owned(), profile.to_owned()],
                offered_rate: None,
            });
        }
    }
    // The same comparison under a FIXED arrival rate. This is the one that can
    // say anything about latency: the closed-loop rows above cannot separate
    // "lower latency" from "higher throughput", because with saturating
    // workers the first follows arithmetically from the second.
    if let Some(rate) = offered_rate {
        for profile in PROFILES {
            for concurrency in &concurrencies {
                scenarios.push(Scenario {
                    label: format!("profile-{profile}-openloop-c{concurrency}"),
                    mode: Mode::Http,
                    protocol: Protocol::Json,
                    keep_alive: true,
                    durability: "wal".to_owned(),
                    concurrency: *concurrency,
                    batch,
                    spans,
                    server_args: vec!["--profile".to_owned(), profile.to_owned()],
                    offered_rate: Some(rate),
                });
            }
        }
    }

    // One scenario per --variant, at each concurrency, on the same base as the
    // profile rows so a swept value is directly comparable with them.
    for flags in &variants {
        let label = variant_label(flags);
        for concurrency in &concurrencies {
            scenarios.push(Scenario {
                label: format!("variant-{label}-c{concurrency}"),
                mode: Mode::Http,
                protocol: Protocol::Json,
                keep_alive: true,
                durability: "wal".to_owned(),
                concurrency: *concurrency,
                batch,
                spans,
                server_args: flags.clone(),
                offered_rate: None,
            });
        }
        if let Some(rate) = offered_rate {
            for concurrency in &concurrencies {
                scenarios.push(Scenario {
                    label: format!("variant-{label}-openloop-c{concurrency}"),
                    mode: Mode::Http,
                    protocol: Protocol::Json,
                    keep_alive: true,
                    durability: "wal".to_owned(),
                    concurrency: *concurrency,
                    batch,
                    spans,
                    server_args: flags.clone(),
                    offered_rate: Some(rate),
                });
            }
        }
    }

    // Applied last so a swept flag beats a scenario's own (the server itself
    // resolves duplicates last-wins, and this keeps that predictable).
    if !server_args.is_empty() {
        for scenario in &mut scenarios {
            scenario.server_args.extend(server_args.iter().cloned());
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
    // No H1 here: this block is spliced into a curated document between the
    // generated markers, which supplies its own title and analysis.
    let _ = writeln!(
        report,
        "Every row is the MEDIAN of {runs} runs, each on a fresh data directory. \
Scenarios are run ROUND-ROBIN rather than one at a time, and their order is \
ROTATED each round, so each scenario's repeats are spread across the whole \
wall-clock window and across positions within a round. Background load then \
hits all of them alike instead of landing on whichever ran during a spike or \
whichever is pinned to the same phase of a periodic load. \
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
        "Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per \
request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS \
of each percentile. Read it with the load model in mind: this is a closed-loop \
generator with a fixed number of workers, all saturating, so latency includes queueing \
and by Little's law tracks concurrency divided by throughput. Latencies are therefore \
only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for \
a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue \
behind.\n"
    );
    let _ = writeln!(
        report,
        "| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms |"
    );
    let _ = writeln!(report, "|---|---|---|---:|---:|---:|---:|---:|---:|---:|");

    let selected: Vec<&Scenario> = scenarios
        .iter()
        .filter(|scenario| match &only {
            Some(filter) => scenario.label.to_ascii_lowercase().contains(filter),
            None => true,
        })
        .collect();

    // Payloads are shared across every scenario that wants the same wire
    // format, so the corpus is built at most twice rather than once per
    // scenario. Round-robin scheduling (below) needs them all resident at
    // once, and re-encoding per scenario would cost more than it saves.
    //
    // Generation is outside every measured window; its cost is reported so
    // the client's share of the work is visible rather than quietly excluded.
    let mut corpus: Vec<(Protocol, Arc<Vec<Vec<u8>>>)> = Vec::new();
    for protocol in [Protocol::Json, Protocol::Protobuf] {
        if !selected
            .iter()
            .any(|scenario| scenario.mode == Mode::Http && scenario.protocol == protocol)
        {
            continue;
        }
        let started = Instant::now();
        let payloads: Vec<Vec<u8>> = (0..spans)
            .step_by(batch)
            .map(|start| {
                let end = (start + batch).min(spans);
                match protocol {
                    Protocol::Json => json_batch(start, end),
                    Protocol::Protobuf => protobuf_batch(start, end),
                }
            })
            .collect();
        let bytes: usize = payloads.iter().map(Vec::len).sum();
        println!(
            "client encode ({}): {:.2}s for {} batches ({:.1} MiB, {:.0} spans/s if it were the limit)",
            protocol.as_str(),
            started.elapsed().as_secs_f64(),
            payloads.len(),
            bytes as f64 / (1024.0 * 1024.0),
            spans as f64 / started.elapsed().as_secs_f64()
        );
        corpus.push((protocol, Arc::new(payloads)));
    }
    println!();

    /// What one scenario has accumulated across the rounds so far.
    #[derive(Default)]
    struct Accumulated {
        rates: Vec<f64>,
        p50s: Vec<f64>,
        p95s: Vec<f64>,
        p99s: Vec<f64>,
        metrics: String,
        failed: Option<String>,
    }

    let mut accumulated: Vec<Accumulated> = (0..selected.len())
        .map(|_| Accumulated::default())
        .collect();

    // ROUND-ROBIN, not scenario-at-a-time. Running all of scenario A's repeats
    // and then all of scenario B's makes the comparison a hostage to whatever
    // else the machine was doing during each block: a load spike lands
    // entirely on one configuration and shows up as a difference between
    // configurations. Interleaving spreads every scenario's repeats across the
    // whole wall-clock window, so drift and contention hit all of them alike
    // and the median across rounds is comparing like with like. It costs
    // nothing but ordering, and it is what makes a run on a machine that is
    // not perfectly idle worth reporting at all.
    for round in 1..=runs {
        println!("--- round {round} of {runs} ---");
        // Rotate the starting point each round. Round-robin alone equalizes
        // across rounds but leaves POSITION WITHIN a round fixed, and
        // background load on a shared machine oscillates on a timescale close
        // to one round — so a scenario pinned to the same slot can sit in the
        // same phase of that oscillation every time, which is a bias that
        // looks exactly like a property of the configuration. Rotating means
        // each scenario occupies a different slot each round.
        //
        // Rotation rather than a shuffle: it is deterministic, needs no RNG
        // (this crate has two dependencies and neither is one), and spreads
        // positions evenly instead of merely randomly.
        let offset = (round - 1) % selected.len().max(1);
        for step in 0..selected.len() {
            let index = (step + offset) % selected.len();
            let scenario = selected[index];
            if accumulated[index].failed.is_some() {
                continue;
            }
            let payloads = corpus
                .iter()
                .find(|(protocol, _)| *protocol == scenario.protocol)
                .map(|(_, payloads)| Arc::clone(payloads))
                .unwrap_or_default();
            let result = if scenario.mode == Mode::Direct {
                run_direct(scenario)
            } else {
                run_http(scenario, &payloads)
            };
            match result {
                Ok(result) if result.refused > 0 => {
                    accumulated[index].failed = Some(format!(
                        "server shed {} connections; the rate is not sustained",
                        result.refused
                    ));
                }
                Ok(result) if result.stored < scenario.spans as u64 => {
                    accumulated[index].failed = Some(format!(
                        "stored {} of {} spans",
                        result.stored, scenario.spans
                    ));
                }
                Ok(result) => {
                    println!(
                        "  {}: {:.0} spans/s ({:.2}s), latency p50 {:.2} / p95 {:.2} / p99 {:.2} / max {:.2} ms",
                        scenario.label,
                        result.rate,
                        result.elapsed.as_secs_f64(),
                        result.latency.p50(),
                        result.latency.p95(),
                        result.latency.p99(),
                        result.latency.max_ms()
                    );
                    let entry = &mut accumulated[index];
                    entry.rates.push(result.rate);
                    entry.p50s.push(result.latency.p50());
                    entry.p95s.push(result.latency.p95());
                    entry.p99s.push(result.latency.p99());
                    entry.metrics = result.metrics;
                }
                Err(error) => accumulated[index].failed = Some(error),
            }
            if let Some(error) = &accumulated[index].failed {
                println!("  {}: FAILED: {error}", scenario.label);
            }
        }
        println!();
    }

    for (index, scenario) in selected.iter().enumerate() {
        let entry = &accumulated[index];
        println!("{} — {}", scenario.label, scenario.describe());
        if let Some(error) = &entry.failed {
            println!("  FAILED: {error}\n");
            let _ = writeln!(
                report,
                "| {} | {} | {} | {} | FAILED: {error} | | | | | |",
                scenario.label,
                scenario.protocol.as_str(),
                scenario.keep_alive,
                scenario.concurrency
            );
            continue;
        }

        let min = entry.rates.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = entry
            .rates
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let mid = median(&mut entry.rates.clone());
        let p50 = median(&mut entry.p50s.clone());
        let p95 = median(&mut entry.p95s.clone());
        let p99 = median(&mut entry.p99s.clone());
        println!("  median: {mid:.0} spans/s (min {min:.0}, max {max:.0})");
        println!(
            "  latency (median of per-run percentiles): p50 {p50:.2} ms, p95 {p95:.2} ms, p99 {p99:.2} ms"
        );
        let stages = stage_summary(&entry.metrics);
        if !stages.is_empty() {
            println!("  stages (server-side totals, last round):");
            println!("{stages}");
        }
        println!();

        let _ = writeln!(
            report,
            "| {} | {} | {} | {} | **{mid:.0}** | {min:.0} | {max:.0} | {p50:.2} | {p95:.2} | {p99:.2} |",
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

    write_report(&report)?;
    Ok(())
}

const GENERATED_BEGIN: &str = "<!-- BEGIN GENERATED -->";
const GENERATED_END: &str = "<!-- END GENERATED -->";

/// Writes the report, preserving hand-written analysis.
///
/// This used to overwrite `INGEST-BENCHMARK.md` wholesale, which silently
/// destroyed every paragraph of interpretation in it — the stage decomposition,
/// the reasoning about what the numbers mean — each time anyone re-ran the
/// benchmark. The generated table is an INPUT to that document, not the
/// document. When the file marks a generated region, only that region is
/// replaced; otherwise the file is written whole, so a fresh checkout still
/// gets a complete report.
fn write_report(report: &str) -> Result<(), String> {
    let path = "INGEST-BENCHMARK.md";
    if let Ok(existing) = std::fs::read_to_string(path) {
        if let (Some(start), Some(end)) =
            (existing.find(GENERATED_BEGIN), existing.find(GENERATED_END))
        {
            if start < end {
                let mut merged = String::with_capacity(existing.len() + report.len());
                merged.push_str(&existing[..start]);
                merged.push_str(GENERATED_BEGIN);
                merged.push('\n');
                merged.push_str(report);
                merged.push_str(&existing[end..]);
                std::fs::write(path, merged).map_err(|e| e.to_string())?;
                println!("Updated the generated section of {path}; prose preserved");
                return Ok(());
            }
        }
        // A file with no markers is analysis this harness must not clobber.
        let aside = "INGEST-BENCHMARK.generated.md";
        std::fs::write(aside, report).map_err(|e| e.to_string())?;
        println!("{path} carries no {GENERATED_BEGIN} marker, so it was left alone; wrote {aside}");
        return Ok(());
    }
    let fresh = format!("# Traza Ingest Benchmark\n\n{GENERATED_BEGIN}\n{report}{GENERATED_END}\n");
    std::fs::write(path, fresh).map_err(|e| e.to_string())?;
    println!("Wrote {path}");
    Ok(())
}
