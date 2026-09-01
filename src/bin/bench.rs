//! Benchmark driver for Traza storage operations.
//!
//! **This run gates itself.** Acceptance gate 6 of the segment format
//! (`docs/segment-format.md`, "Acceptance gates" — the deliberately amended,
//! absolute form) sets tripwires on the canonical 1M-span corpus:
//! **trace-lookup p50 at or below 0.75 ms** and **attribute-filter p50 at or
//! below 6 ms**. They are asserted here after measurement and before
//! `docs/benchmarks/canonical-corpus.md` is written: a canonical run that
//! misses either exits non-zero and writes nothing, because the harness does
//! not publish a number the format's own contract says is a failure.
//! Non-canonical configurations — an overridden corpus size
//! (`TRAZA_BENCH_SPANS`) or compaction knob (`TRAZA_BENCH_COMPACTION_FANOUT`,
//! `TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES`) — never write the doc and are
//! not gated: the tripwires are defined on the canonical corpus under the
//! default configuration only, and an experiment prints its numbers and
//! leaves the record alone.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SPAN_COUNT: usize = 1_000_000;
/// The compaction fan-out the published record is measured at.
const DEFAULT_COMPACTION_FANOUT: &str = "4";
const BATCH_SIZE: usize = 1_000;
const TRACE_SAMPLES: usize = 200;
const FILTER_SAMPLES: usize = 100;
/// The `limit` every filter sample carries — part of the oracle's arithmetic,
/// so it is a named constant the query path and the verification share.
const FILTER_LIMIT: usize = 100;

/// Acceptance gate 6 (`docs/segment-format.md`, "Acceptance gates", as
/// amended to absolute tripwires): median tripwires on the canonical corpus.
/// A canonical run past either exits non-zero and writes nothing.
const GATE_TRACE_P50: Duration = Duration::from_micros(750);
const GATE_FILTER_P50: Duration = Duration::from_millis(6);

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

/// Compaction fan-out for the benchmarked server; `0` disables compaction.
fn compaction_fanout() -> String {
    std::env::var("TRAZA_BENCH_COMPACTION_FANOUT")
        .unwrap_or_else(|_| DEFAULT_COMPACTION_FANOUT.to_owned())
}

/// The segment ceiling the published record is measured at: the production
/// default, read from the config rather than repeated as a literal.
fn default_max_segment_bytes() -> String {
    traza::CompactionConfig::default()
        .max_segment_bytes
        .to_string()
}

/// Size ceiling for compacted segments. This bounds the segment count from
/// below (roughly corpus / cap), and filtered search probes every segment, so
/// it is the knob scaling experiments need to vary. Defaults to the real
/// production default rather than a literal, so the two cannot drift apart.
fn compaction_max_segment_bytes() -> String {
    std::env::var("TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES")
        .unwrap_or_else(|_| default_max_segment_bytes())
}

fn span_count() -> Result<usize, Box<dyn std::error::Error>> {
    // TRAZA_BENCH_SPANS overrides the corpus size for scaling experiments.
    // The published record is only rewritten for the canonical configuration,
    // so experimental runs cannot silently change its numbers. A set-but-
    // unparseable value is an error, not a silent fall-through: falling back
    // would run the full canonical corpus — and rewrite the record — under a
    // configuration the operator explicitly asked to change.
    match std::env::var("TRAZA_BENCH_SPANS") {
        Ok(value) => value.parse().map_err(|_| {
            format!("TRAZA_BENCH_SPANS is set to {value:?}, which is not a span count").into()
        }),
        Err(_) => Ok(DEFAULT_SPAN_COUNT),
    }
}

/// True only for the configuration the published record is defined on: the
/// canonical corpus at the default compaction knobs. Gate 6 and the rewrite of
/// `docs/benchmarks/canonical-corpus.md` both key on this — an uncompacted-
/// baseline experiment (`TRAZA_BENCH_COMPACTION_FANOUT=0`) runs the same
/// corpus through hundreds of segments, so gating it against tripwires the
/// spec defines for the default configuration would fail it spuriously, and
/// writing its numbers over the record would be worse.
fn canonical_configuration(span_count: usize) -> bool {
    span_count == DEFAULT_SPAN_COUNT
        && compaction_fanout() == DEFAULT_COMPACTION_FANOUT
        && compaction_max_segment_bytes() == default_max_segment_bytes()
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
        // Measure the DEFAULT contract. Benchmarking `buffered` would report a
        // number no production deployment can rely on.
        .arg("--durability")
        .arg("wal")
        // Compaction is on by default; TRAZA_BENCH_COMPACTION_FANOUT=0 measures
        // the uncompacted baseline through this same harness, so the two are
        // directly comparable rather than measured by different clients.
        .arg("--compaction-fanout")
        .arg(compaction_fanout())
        .arg("--compaction-max-segment-bytes")
        .arg(compaction_max_segment_bytes())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut server = ServerGuard { child, data_dir };
    wait_for_server(port, &mut server.child)?;

    #[allow(non_snake_case)]
    let SPAN_COUNT = span_count()?;
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

    wait_for_record_count(port, SPAN_COUNT as u64)?;

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
        let path = format!("/v1/spans?attr.benchmark.group=group-{group}&limit={FILTER_LIMIT}");
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
        // The semantic oracle, before the latency is recorded: a wrong
        // answer aborts the run — the same refuse-to-publish pattern as the
        // gates — because a latency measured on a wrong answer is not a
        // measurement of the store.
        verify_filter_response(&response.1, group, SPAN_COUNT, FILTER_LIMIT)
            .map_err(|error| format!("filter oracle failed, refusing to publish: {error}"))?;
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

    // Acceptance gate 6's tripwires, asserted before the record is written.
    // Only the canonical configuration — default corpus, default compaction
    // knobs — defines the gate, and only the canonical configuration writes
    // the doc; the two share one predicate so they cannot drift apart.
    let canonical = canonical_configuration(SPAN_COUNT);
    if canonical {
        let mut gate_misses = Vec::new();
        if trace_p50 > GATE_TRACE_P50 {
            gate_misses.push(format!(
                "trace-lookup p50 measured {:.3} ms, tripwire is <= {:.3} ms",
                ms(trace_p50),
                ms(GATE_TRACE_P50),
            ));
        }
        if filter_p50 > GATE_FILTER_P50 {
            gate_misses.push(format!(
                "attribute-filter p50 measured {:.3} ms, tripwire is <= {:.3} ms",
                ms(filter_p50),
                ms(GATE_FILTER_P50),
            ));
        }
        if !gate_misses.is_empty() {
            for miss in &gate_misses {
                eprintln!("GATE FAILED: {miss}");
            }
            return Err("acceptance gate 6 (docs/segment-format.md) failed; \
                 docs/benchmarks/canonical-corpus.md was not written"
                .into());
        }
    }

    let context = machine_context();
    let measured_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut report = format!(
        "# Traza Benchmarks\n\n\
These values were measured by `cargo run --release --bin bench`; they are not estimates. The benchmark builds and starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory.\n\n\
## Results\n\n\
| Metric | Measured | Target | Result |\n\
|---|---:|---:|---|\n\
| Sustained batched HTTP ingest (durability=wal, compaction fanout={fanout}, max segment bytes={max_segment_bytes}) | {ingest_rate:.0} spans/s | >= 50,000 spans/s | {} |\n\
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
- Filter sampling: {FILTER_SAMPLES} deterministic `attr.benchmark.group` queries with `limit={FILTER_LIMIT}`; each response is verified against the corpus construction — exact span count, the requested group value on every span, and the exact expected span ids — before its latency is recorded. A wrong answer aborts the run without writing this file.\n\
- Percentiles: nearest-rank selection over complete request wall-clock durations measured with `std::time::Instant`; no warm-up samples are discarded.\n\
- Build: Cargo release profile. Timestamp: Unix {measured_at}.\n\
- Machine context: {context}.\n\
- Load conditions: 1-minute load average {loadavg} at the end of the run — ambient desktop load, not an idle host (the house rule is [ingest.md's](ingest.md#load-conditions)). An idle rerun will likely improve the tails; the gate tripwires, not these point estimates, are the contract.\n\
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
        fanout = compaction_fanout(),
        max_segment_bytes = compaction_max_segment_bytes(),
        loadavg = load_average_1m(),
    );
    report.push_str(&format!(
        "\n## Verification Notes\n\n\
- Corpus declaration: `1000000` spans (1,000,000 spans).\n\
- Every reported result is measured by this benchmark run, never estimated.\n\
- Unsuccessful lookups are reported as misses.\n\
- **This file exists only because the run passed acceptance gate 6's tripwires** \
([segment format](../segment-format.md#acceptance-gates), as amended to absolute bounds): \
trace-lookup p50 <= {:.2} ms and attribute-filter p50 <= {:.0} ms on this canonical corpus. \
The benchmark asserts them after measuring and before writing; a run that misses either exits \
non-zero and writes nothing.\n",
        ms(GATE_TRACE_P50),
        ms(GATE_FILTER_P50),
    ));
    if canonical {
        fs::write("docs/benchmarks/canonical-corpus.md", report)?;
    } else {
        println!(
            "(experimental configuration — docs/benchmarks/canonical-corpus.md not rewritten, \
             gate 6 not asserted: the tripwires are defined on the canonical corpus at the \
             default compaction configuration only)"
        );
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
    if canonical {
        println!("Wrote docs/benchmarks/canonical-corpus.md");
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

fn wait_for_record_count(port: u16, expected: u64) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = request(port, "GET", "/v1/stats", None)?;
        if status == 200 {
            let value: Value = serde_json::from_slice(&body)?;
            let count = value
                .get("record_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
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

/// The 1-minute load average, measured rather than asserted, for the
/// load-conditions line of the record. Reads `/proc/loadavg` where it exists
/// and falls back to `sysctl -n vm.loadavg` (macOS); anything else reports
/// "unavailable" instead of a guess.
fn load_average_1m() -> String {
    if let Ok(contents) = fs::read_to_string("/proc/loadavg") {
        if let Some(first) = contents.split_whitespace().next() {
            return first.to_owned();
        }
    }
    if let Ok(output) = Command::new("sysctl").args(["-n", "vm.loadavg"]).output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(first) = text
                .split_whitespace()
                .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
            {
                return first.to_owned();
            }
        }
    }
    "unavailable".to_owned()
}

/// The filter samples' semantic oracle.
///
/// The corpus construction fixes each query's exact answer: spans carrying
/// `benchmark.group = group-{g}` are exactly the indices `g + 100k` below
/// `span_count`, the stable span order is ascending start time — which is
/// ascending index by construction — and a limited query returns the first
/// `limit` of them. So the oracle demands the exact span count the corpus
/// defines, the requested group value on every returned span, and the exact
/// set of span ids. Anything less gated latency on "200 and parseable",
/// which a wrong (even empty) answer satisfies.
fn verify_filter_response(
    body: &[u8],
    group: usize,
    span_count: usize,
    limit: usize,
) -> Result<(), String> {
    let parsed: Value = serde_json::from_slice(body)
        .map_err(|error| format!("group-{group} response is not JSON: {error}"))?;
    let spans = parsed
        .get("spans")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("group-{group} response carries no spans array"))?;
    // The generator writes `benchmark.group = group-{i % 100}`, so the
    // group's population is the count of indices `≡ group (mod 100)` below
    // the corpus size — NOT a constant: a scaled-down corpus has short
    // groups, and the last groups run one shorter whenever 100 does not
    // divide the corpus.
    let population = if group < span_count {
        (span_count - group - 1) / 100 + 1
    } else {
        0
    };
    let expected = population.min(limit);
    if spans.len() != expected {
        return Err(format!(
            "group-{group} returned {} spans, the corpus defines exactly {expected}",
            spans.len()
        ));
    }
    // Start times ascend with the index, so the limited query's answer is
    // the FIRST `expected` matches: indices `group + 100k`, whose span ids
    // the generator derives as `{:016x}` of index + 1. Exact set, order
    // left free.
    let mut expected_ids: BTreeSet<String> = (0..expected)
        .map(|position| format!("{:016x}", group + 100 * position + 1))
        .collect();
    let group_value = format!("group-{group}");
    for span in spans {
        let held = span
            .get("attributes")
            .and_then(|attributes| attributes.get("benchmark.group"))
            .and_then(Value::as_str);
        if held != Some(group_value.as_str()) {
            return Err(format!(
                "group-{group} returned a span carrying benchmark.group {held:?}"
            ));
        }
        let span_id = span
            .get("span_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("group-{group} returned a span without a span_id"))?;
        if !expected_ids.remove(span_id) {
            return Err(format!(
                "group-{group} returned span {span_id}, which is not among the \
                 expected ids (or is a duplicate)"
            ));
        }
    }
    // Counts matched and every returned id consumed a distinct expected id,
    // so the sets are equal.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The honest response for one group, built from the corpus rule
    /// independently of the oracle's own arithmetic: matching indices are
    /// `group + 100k` below `span_count`, ids are `{:016x}` of index + 1,
    /// and the limited query returns the first `limit` in span order.
    fn honest_response(group: usize, span_count: usize, limit: usize) -> Vec<u8> {
        let spans: Vec<Value> = (group..span_count)
            .step_by(100)
            .take(limit)
            .map(|index| honest_span(index, &format!("group-{group}")))
            .collect();
        serde_json::to_vec(&json!({"spans": spans})).expect("response encodes")
    }

    fn honest_span(index: usize, group_value: &str) -> Value {
        json!({
            "trace_id": format!("{:032x}", index / 10 + 1),
            "span_id": format!("{:016x}", index + 1),
            "attributes": {"benchmark.group": group_value},
        })
    }

    #[test]
    fn the_oracle_accepts_the_generators_exact_answer() {
        verify_filter_response(&honest_response(7, 1_000_000, 100), 7, 1_000_000, 100)
            .expect("the honest canonical answer passes");
    }

    #[test]
    fn the_oracle_derives_the_expected_count_from_the_corpus() {
        // 250 spans: group 30 holds indices 30, 130, 230 — three spans,
        // nothing like the canonical 100. The oracle must derive that from
        // the generator's rule, not assume the canonical fill.
        verify_filter_response(&honest_response(30, 250, 100), 30, 250, 100)
            .expect("a short group's exact population passes");
        // The same three spans claimed against a corpus of 230 — where the
        // group holds only indices 30 and 130 — are a wrong answer.
        verify_filter_response(&honest_response(30, 250, 100), 30, 230, 100)
            .expect_err("a count the corpus does not define is refused");
    }

    #[test]
    fn the_oracle_rejects_a_parseable_empty_answer() {
        let body = serde_json::to_vec(&json!({"spans": []})).expect("encodes");
        let refusal = verify_filter_response(&body, 7, 1_000_000, 100)
            .expect_err("an empty answer is wrong, however parseable");
        assert!(
            refusal.contains("0 spans"),
            "the refusal names the wrong count: {refusal}"
        );
    }

    #[test]
    fn the_oracle_rejects_a_span_from_the_wrong_group() {
        let mut spans: Vec<Value> = (7..1_000_000)
            .step_by(100)
            .take(100)
            .map(|index| honest_span(index, "group-7"))
            .collect();
        spans[41] = honest_span(4_107, "group-8");
        let body = serde_json::to_vec(&json!({"spans": spans})).expect("encodes");
        verify_filter_response(&body, 7, 1_000_000, 100)
            .expect_err("a span carrying another group's value is refused");
    }

    #[test]
    fn the_oracle_rejects_substituted_span_ids() {
        // Right count, right group value on every span — but the wrong
        // rows: the window is shifted by one match, so the first expected
        // id is missing and one id past the limit appears instead.
        let spans: Vec<Value> = (107..1_000_000)
            .step_by(100)
            .take(100)
            .map(|index| honest_span(index, "group-7"))
            .collect();
        let body = serde_json::to_vec(&json!({"spans": spans})).expect("encodes");
        verify_filter_response(&body, 7, 1_000_000, 100)
            .expect_err("the exact expected ids are part of the answer");
    }
}
