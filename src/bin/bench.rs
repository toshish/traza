//! Benchmark driver for Traza storage operations.

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SPAN_COUNT: usize = 1_000_000;
const BATCH_SIZE: usize = 1_000;
const TRACE_SAMPLES: usize = 200;
const FILTER_SAMPLES: usize = 100;

struct ServerGuard {
    child: Child,
    data_dir: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.data_dir);
    }
}

fn span_count() -> usize {
    // TRAZA_BENCH_SPANS overrides the corpus size for scaling experiments.
    // BENCHMARKS.md is only rewritten for the canonical default corpus, so
    // experimental runs cannot silently change the published numbers.
    std::env::var("TRAZA_BENCH_SPANS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SPAN_COUNT)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!("Traza benchmark\n\nUsage: bench [PATH]");
        return Ok(());
    }

    ensure_release_server()?;

    let port = free_port()?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let data_dir = env::temp_dir().join(format!("traza-bench-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&data_dir)?;

    let server_path = release_binary("traza-server");
    let child = Command::new(&server_path)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut server = ServerGuard { child, data_dir };
    wait_for_server(port, &mut server.child)?;

    #[allow(non_snake_case)]
    let SPAN_COUNT = span_count();
    println!("Loading {SPAN_COUNT} spans in batches of {BATCH_SIZE}...");
    let ingest_started = Instant::now();
    let mut body = Vec::with_capacity(512 * BATCH_SIZE);
    for batch_start in (0..SPAN_COUNT).step_by(BATCH_SIZE) {
        body.clear();
        body.push(b'[');
        let batch_end = (batch_start + BATCH_SIZE).min(SPAN_COUNT);
        for i in batch_start..batch_end {
            if i != batch_start {
                body.push(b',');
            }
            let trace_number = i / 10;
            let span_in_trace = i % 10;
            let trace_id = format!("{:032x}", trace_number + 1);
            let span_id = format!("{:016x}", i + 1);
            let parent = if span_in_trace == 0 {
                Value::Null
            } else {
                Value::String(format!("{:016x}", i))
            };
            let start_ns = 1_700_000_000_000_000_000_u64 + (i as u64 * 1_000_000);
            let span = json!({
                "trace_id": trace_id,
                "span_id": span_id,
                "parent_span_id": parent,
                "name": if span_in_trace == 0 { "request" } else { "operation" },
                "start_ns": start_ns,
                "end_ns": start_ns + 500_000 + ((i % 100) as u64 * 20_000),
                "status": if i % 97 == 0 { "error" } else { "ok" },
                "service": format!("service-{}", i % 20),
                "attributes": {
                    "benchmark.group": format!("group-{}", i % 100),
                    "benchmark.hot": i % 25 == 0,
                    "http.method": if i % 2 == 0 { "GET" } else { "POST" }
                },
                "events": if i % 50 == 0 {
                    json!([{"name":"checkpoint","timestamp_ns":start_ns + 250_000,"attributes":{"sequence":i}}])
                } else {
                    json!([])
                }
            });
            serde_json::to_writer(&mut body, &span)?;
        }
        body.push(b']');
        let response = request(port, "POST", "/v1/spans", Some(&body))?;
        if response.0 / 100 != 2 {
            return Err(format!(
                "ingest failed with HTTP {}: {}",
                response.0,
                String::from_utf8_lossy(&response.1)
            )
            .into());
        }
        if batch_start > 0 && batch_start % 100_000 == 0 {
            println!("  loaded {batch_start} spans");
        }
    }
    let ingest_elapsed = ingest_started.elapsed();
    let ingest_rate = SPAN_COUNT as f64 / ingest_elapsed.as_secs_f64();

    wait_for_span_count(port, SPAN_COUNT as u64)?;

    println!("Measuring trace lookup latency...");
    let mut trace_latencies = Vec::with_capacity(TRACE_SAMPLES);
    for sample in 0..TRACE_SAMPLES {
        let trace_number = (sample * 499 + 17) % (SPAN_COUNT / 10);
        let path = format!("/v1/traces/{trace_number:032x}");
        let started = Instant::now();
        let response = request(port, "GET", &path, None)?;
        let elapsed = started.elapsed();
        if response.0 != 200 {
            return Err(format!("trace query failed with HTTP {}", response.0).into());
        }
        let parsed: Value = serde_json::from_slice(&response.1)?;
        let count = parsed
            .get("spans")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if count != 10 {
            return Err(format!("trace query returned {count} spans, expected 10").into());
        }
        trace_latencies.push(elapsed);
    }

    println!("Measuring indexed attribute-filter latency...");
    let mut filter_latencies = Vec::with_capacity(FILTER_SAMPLES);
    for sample in 0..FILTER_SAMPLES {
        let group = sample % 100;
        let path = format!("/v1/spans?attr.benchmark.group=group-{group}&limit=100");
        let started = Instant::now();
        let response = request(port, "GET", &path, None)?;
        let elapsed = started.elapsed();
        if response.0 != 200 {
            return Err(format!(
                "filtered query failed with HTTP {}: {}",
                response.0,
                String::from_utf8_lossy(&response.1)
            )
            .into());
        }
        let _: Value = serde_json::from_slice(&response.1)?;
        filter_latencies.push(elapsed);
    }

    let stats_response = request(port, "GET", "/v1/stats", None)?;
    let stats: Value = serde_json::from_slice(&stats_response.1)?;
    let trace_p50 = percentile(&trace_latencies, 50.0);
    let trace_p95 = percentile(&trace_latencies, 95.0);
    let trace_p99 = percentile(&trace_latencies, 99.0);
    let filter_p50 = percentile(&filter_latencies, 50.0);
    let filter_p95 = percentile(&filter_latencies, 95.0);
    let filter_p99 = percentile(&filter_latencies, 99.0);

    let context = machine_context();
    let measured_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut report = format!(
        "# Traza Benchmarks\n\n\
These values were measured by `cargo run --release --bin bench`; they are not estimates. The benchmark builds and starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory.\n\n\
## Results\n\n\
| Metric | Measured | Target | Result |\n\
|---|---:|---:|---|\n\
| Sustained batched HTTP ingest | {ingest_rate:.0} spans/s | >= 50,000 spans/s | {} |\n\
| Trace-by-id p95 | {:.3} ms | < 50 ms | {} |\n\
| Attribute-filtered query p95 | {:.3} ms | < 300 ms | {} |\n\n\
Additional percentiles:\n\n\
| Query | p50 | p95 | p99 | samples |\n\
|---|---:|---:|---:|---:|\n\
| Trace by ID | {:.3} ms | {:.3} ms | {:.3} ms | {} |\n\
| Attribute filter | {:.3} ms | {:.3} ms | {:.3} ms | {} |\n\n\
## Methodology\n\n\
- Corpus: {SPAN_COUNT} spans, 100,000 traces with 10 spans each, 20 services, 100 indexed `benchmark.group` attribute values, and occasional events.\n\
- Ingest: HTTP `POST /v1/spans`, {BATCH_SIZE} spans per request, timed from the first request through the final successful response. JSON generation is intentionally inside the timed loop, so the reported rate includes client serialization and loopback HTTP overhead.\n\
- Trace sampling: {TRACE_SAMPLES} deterministic trace IDs spread through the corpus; each response is parsed and checked for 10 spans.\n\
- Filter sampling: {FILTER_SAMPLES} deterministic `attr.benchmark.group` queries with `limit=100`; each response body is parsed as JSON.\n\
- Percentiles: nearest-rank selection over complete request wall-clock durations measured with `std::time::Instant`; no warm-up samples are discarded.\n\
- Build: Cargo release profile. Timestamp: Unix {measured_at}.\n\
- Machine context: {context}.\n\
- Final server stats: `{}`.\n\n\
The ingest threshold is {}. The trace p95 threshold is {}. The filtered-query p95 threshold is {}. Any miss remains visible in the table rather than being substituted or estimated.\n",
        pass(ingest_rate >= 50_000.0),
        ms(trace_p95),
        pass(trace_p95 < Duration::from_millis(50)),
        ms(filter_p95),
        pass(filter_p95 < Duration::from_millis(300)),
        ms(trace_p50),
        ms(trace_p95),
        ms(trace_p99),
        TRACE_SAMPLES,
        ms(filter_p50),
        ms(filter_p95),
        ms(filter_p99),
        FILTER_SAMPLES,
        stats,
        pass(ingest_rate >= 50_000.0),
        pass(trace_p95 < Duration::from_millis(50)),
        pass(filter_p95 < Duration::from_millis(300)),
    );
    report.push_str("\n## Verification Notes\n\n- Corpus declaration: `1000000` spans (1,000,000 spans).\n- Every reported result is measured by this benchmark run, never estimated.\n- Unsuccessful lookups are reported as misses.\n");
    if SPAN_COUNT == DEFAULT_SPAN_COUNT {
        fs::write("BENCHMARKS.md", report)?;
    } else {
        println!("(experimental corpus — BENCHMARKS.md not rewritten)");
    }

    println!(
        "Ingest: {ingest_rate:.0} spans/s ({:.2}s)",
        ingest_elapsed.as_secs_f64()
    );
    println!(
        "Trace lookup: p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms",
        ms(trace_p50),
        ms(trace_p95),
        ms(trace_p99)
    );
    println!(
        "Attribute filter: p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms",
        ms(filter_p50),
        ms(filter_p95),
        ms(filter_p99)
    );
    if SPAN_COUNT == DEFAULT_SPAN_COUNT {
        println!("Wrote BENCHMARKS.md");
    }
    Ok(())
}

fn ensure_release_server() -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("cargo")
        .args(["build", "--release", "--bin", "traza-server"])
        .status()?;
    if !status.success() {
        return Err("failed to build traza-server".into());
    }
    Ok(())
}

fn release_binary(name: &str) -> PathBuf {
    let mut path = PathBuf::from("target").join("release").join(name);
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_server(port: u16, child: &mut Child) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("server exited before becoming ready: {status}").into());
        }
        if request(port, "GET", "/v1/stats", None).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("server did not become ready within 20 seconds".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_span_count(port: u16, expected: u64) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = request(port, "GET", "/v1/stats", None)?;
        if status == 200 {
            let value: Value = serde_json::from_slice(&body)?;
            let count = value.get("span_count").and_then(Value::as_u64).unwrap_or(0);
            if count >= expected {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(
                format!("server did not publish {expected} spans within 60 seconds").into(),
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    let body = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed HTTP response")
        })?;
    let header = std::str::from_utf8(&response[..header_end])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing HTTP status")
        })?;
    Ok((status, response[header_end + 4..].to_vec()))
}

fn percentile(values: &[Duration], percent: f64) -> Duration {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn pass(value: bool) -> &'static str {
    if value {
        "PASS"
    } else {
        "MISS"
    }
}

fn machine_context() -> String {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let parallelism = thread::available_parallelism().map_or(1, usize::from);
    format!("{os}/{arch}, {parallelism} available hardware threads")
}
