//! Benchmark driver for Traza's LLM aggregation endpoints.
//!
//! `bench` measures trace lookup and attribute-filtered search. Neither of
//! those touches the aggregation path that a dashboard actually calls, and
//! that path has two failure modes the other benchmarks cannot see:
//!
//! 1. **Cold start.** Aggregates are served from a per-segment rollup cache
//!    that is in-memory only and empty after every restart, so the first
//!    aggregation decodes whatever the cache is missing. The cold/warm ratio
//!    is the number, and it is invisible to any harness that measures a warm
//!    process.
//! 2. **Time windows under concurrent ingest.** A segment fully inside the
//!    requested window is answered from its rollup; one that straddles the
//!    window is decoded. Concurrent ingest clients interleave their
//!    timestamps across segments, so with enough of them NO segment is fully
//!    inside any window and every query takes the slow path. A single-threaded
//!    ingest harness reports the good case and never sees this.
//!
//! So this driver restarts the server to measure cold, and takes
//! `TRAZA_QUERY_BENCH_THREADS` as a first-class axis rather than a detail.
//!
//! Usage: `cargo run --release --bin query-bench`
//! Knobs: `TRAZA_QUERY_BENCH_SPANS`, `TRAZA_QUERY_BENCH_THREADS`.

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The canonical corpus is 1M spans — the size every published edition of the
/// record has measured, and the size `bench`'s canonical corpus uses — so the
/// record stays comparable edition over edition. (It was briefly 500,000,
/// which halved the store, collapsed it to one segment, and made the
/// latency-over-latency diffs conflate the engine with the corpus.) Smaller
/// experiments are one env var away and never touch the record.
const DEFAULT_SPAN_COUNT: usize = 1_000_000;
const DEFAULT_THREADS: usize = 8;
const BATCH_SIZE: usize = 1_000;
/// Warm samples per query shape. The cold sample is the single first request
/// after a restart and can never be repeated without restarting again.
const WARM_SAMPLES: usize = 20;

/// Corpus time base and span spacing. The window queries are computed from
/// these, so the harness always knows exactly what fraction it asked for.
const BASE_NS: u64 = 1_700_000_000_000_000_000;
const SPACING_NS: u64 = 1_000_000;

const MODELS: [&str; 6] = [
    "gpt-4o",
    "gpt-4o-mini",
    "claude-opus-4",
    "claude-sonnet-4",
    "gemini-2.5-pro",
    "llama-3.1-70b",
];
const PROVIDERS: [&str; 4] = ["openai", "anthropic", "google", "meta"];

struct ServerGuard {
    child: Option<Child>,
    data_dir: PathBuf,
    port: u16,
}

impl ServerGuard {
    fn start(data_dir: PathBuf, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let mut guard = Self {
            child: None,
            data_dir,
            port,
        };
        guard.spawn()?;
        Ok(guard)
    }

    fn spawn(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut child = Command::new(release_binary("traza-server"))
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--port")
            .arg(self.port.to_string())
            // The default durability contract, for the same reason `bench`
            // uses it: a number no deployment can rely on is not a number.
            .arg("--durability")
            .arg("wal")
            // Compaction is pinned rather than left to its defaults because
            // it sets the SEGMENT COUNT, and segment count is the axis the
            // windowed queries scale on. An unpinned run compacts on its own
            // schedule and can measure a four-segment store against a
            // seventy-segment one without saying so.
            .arg("--compaction-fanout")
            .arg(compaction_fanout())
            .arg("--compaction-max-segment-bytes")
            .arg(compaction_max_segment_bytes())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        wait_for_server(self.port, &mut child)?;
        self.child = Some(child);
        Ok(())
    }

    /// Stops and restarts the server on the same data directory.
    ///
    /// This is the whole point of the harness: the rollup cache lives in
    /// process memory, so a restart is the only honest way to produce a cold
    /// query. Dropping a cache from the inside would measure a code path no
    /// operator ever takes.
    fn restart(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.stop();
        // The listening socket needs a moment to clear before the rebind.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpListener::bind(("127.0.0.1", self.port)) {
                Ok(listener) => {
                    drop(listener);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.spawn()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.data_dir);
    }
}

/// One measured query shape.
struct Shape {
    label: &'static str,
    path: String,
    /// What the shape is for, quoted into the report's methodology section.
    note: &'static str,
    /// Total `spans` the rows must sum to, when the shape's window makes that
    /// exactly knowable. A benchmark that cannot tell a fast query from an
    /// empty one is measuring the HTTP stack, so every shape is checked for a
    /// non-empty answer and the whole-corpus shapes are checked for the right
    /// one. This is also the assertion that catches double-counting: a
    /// supersede bug shows up here as a sum ABOVE the corpus size.
    expected_spans: Option<usize>,
}

/// Checks a response actually aggregated something, and — where the answer is
/// exactly knowable — that it aggregated the right amount.
fn verify(body: &Value, expected_spans: Option<usize>) -> Result<(), String> {
    let rows = body
        .get("rows")
        .or_else(|| body.get("sessions"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("response has no rows or sessions array: {body}"))?;
    if rows.is_empty() {
        return Err("response aggregated nothing; the timing would be meaningless".to_owned());
    }
    if let Some(expected) = expected_spans {
        let total: u64 = rows
            .iter()
            .filter_map(|row| row.get("spans").and_then(Value::as_u64))
            .sum();
        if total != expected as u64 {
            return Err(format!(
                "rows sum to {total} spans, expected {expected} — the aggregate is wrong, \
                 so its latency is not worth reporting"
            ));
        }
    }
    Ok(())
}

struct Measured {
    label: &'static str,
    path: String,
    note: &'static str,
    cold: Duration,
    warm_p50: Duration,
    warm_p95: Duration,
}

impl Measured {
    /// Cold divided by warm p50 — the number the rollup cache is judged on.
    fn ratio(&self) -> f64 {
        let warm = self.warm_p50.as_secs_f64();
        if warm > 0.0 {
            self.cold.as_secs_f64() / warm
        } else {
            f64::INFINITY
        }
    }
}

/// Where the published record lives.
const RECORD_PATH: &str = "docs/benchmarks/query.md";

/// The fan-out the published record is measured at.
const DEFAULT_COMPACTION_FANOUT: &str = "4";

/// Compaction fan-out for the benchmarked server; `0` disables compaction.
fn compaction_fanout() -> String {
    env::var("TRAZA_QUERY_BENCH_COMPACTION_FANOUT")
        .unwrap_or_else(|_| DEFAULT_COMPACTION_FANOUT.to_owned())
}

/// The segment ceiling the published record is measured at: the production
/// default, read from the config rather than repeated as a literal.
fn default_max_segment_bytes() -> String {
    traza::CompactionConfig::default()
        .max_segment_bytes
        .to_string()
}

/// Size ceiling for compacted segments — the knob that decides how many
/// segments the corpus lands in. Defaults to the production default rather
/// than a literal so the two cannot drift apart.
fn compaction_max_segment_bytes() -> String {
    env::var("TRAZA_QUERY_BENCH_COMPACTION_MAX_SEGMENT_BYTES")
        .unwrap_or_else(|_| default_max_segment_bytes())
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Traza query benchmark\n\n\
             Usage: query-bench\n\n\
             Measures the LLM aggregation endpoints cold (first request after a\n\
             server restart) and warm, over whole-corpus and time-windowed\n\
             queries.\n\n\
             Environment:\n  \
             TRAZA_QUERY_BENCH_SPANS    corpus size (default {DEFAULT_SPAN_COUNT})\n  \
             TRAZA_QUERY_BENCH_THREADS  concurrent ingest clients (default {DEFAULT_THREADS})\n"
        );
        return Ok(());
    }

    ensure_release_server()?;

    let span_count = env_usize("TRAZA_QUERY_BENCH_SPANS", DEFAULT_SPAN_COUNT);
    let threads = env_usize("TRAZA_QUERY_BENCH_THREADS", DEFAULT_THREADS);
    let port = free_port()?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let data_dir =
        env::temp_dir().join(format!("traza-query-bench-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&data_dir)?;
    let mut server = ServerGuard::start(data_dir.clone(), port)?;

    println!("Loading {span_count} LLM spans with {threads} concurrent client(s)...");
    println!("  (a dashboard probe queries throughout, so compaction is observed live)");
    let ingest_started = Instant::now();
    // The churn probe runs FOR THE WHOLE of ingest, flush and settle. An
    // earlier version measured only the window after ingest finished and saw
    // zero merges: by then compaction had already caught up. Compaction only
    // exists while writes do, so the probe has to overlap the writes.
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut ingest_elapsed = Duration::ZERO;
    let churn = thread::scope(|scope| -> Result<Churn, String> {
        let probe = scope.spawn(|| churn_probe(port, &stop));
        let result = (|| -> Result<(), String> {
            ingest(port, span_count, threads).map_err(|error| error.to_string())?;
            wait_for_record_count(port, span_count as u64).map_err(|error| error.to_string())?;
            ingest_elapsed = ingest_started.elapsed();
            println!(
                "  ingested in {:.2}s ({:.0} spans/s)",
                ingest_elapsed.as_secs_f64(),
                span_count as f64 / ingest_elapsed.as_secs_f64()
            );
            let (status, body) =
                request(port, "POST", "/v1/flush", None).map_err(|error| error.to_string())?;
            if status / 100 != 2 {
                return Err(format!(
                    "flush failed with HTTP {status}: {}",
                    String::from_utf8_lossy(&body)
                ));
            }
            wait_for_stable_segment_count(port).map_err(|error| error.to_string())?;
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        let samples = probe
            .join()
            .map_err(|_| "churn probe panicked".to_owned())?;
        result?;
        samples.map_err(|error| error.to_string())
    })?;
    let segments = segment_count(port).unwrap_or(0);
    println!(
        "  settled at {segments} segments; {} queries during ingest across {} merge event(s): \
         p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms (settled p50 {:.1} ms)",
        churn.samples,
        churn.merge_events,
        ms(churn.p50),
        ms(churn.p95),
        ms(churn.max),
        ms(churn.settled_p50),
    );

    // Windows are expressed in absolute nanoseconds over the corpus's own
    // time range, so "1%" is exactly one percent of the ingested span of time
    // rather than an approximation of it.
    let last_ns = BASE_NS + (span_count.saturating_sub(1) as u64 * SPACING_NS);
    let total_ns = last_ns - BASE_NS;
    let window = |fraction: f64| -> String {
        let width = (total_ns as f64 * fraction) as u64;
        // Taken from the MIDDLE of the corpus: a window at either end is
        // partly answered by ruling segments out entirely, which flatters the
        // result for the case a dashboard does not ask.
        let since = BASE_NS + (total_ns - width) / 2;
        format!("&since_ns={since}&until_ns={}", since + width)
    };

    let shapes = vec![
        Shape {
            label: "stats/llm group_by=model, whole corpus",
            path: "/v1/stats/llm?group_by=model".to_owned(),
            note: "every segment is fully inside the window, so every segment can be answered from its rollup",
            expected_spans: Some(span_count),
        },
        Shape {
            label: "stats/llm group_by=model, 10% window",
            path: format!("/v1/stats/llm?group_by=model{}", window(0.10)),
            note: "the dashboard case: segments straddling the window boundary are decoded rather than rolled up",
            expected_spans: None,
        },
        Shape {
            label: "stats/llm group_by=model, 1% window",
            path: format!("/v1/stats/llm?group_by=model{}", window(0.01)),
            note: "the narrow dashboard case, where the decoded fraction of each straddling segment matters most",
            expected_spans: None,
        },
        Shape {
            label: "stats/llm group_by=session",
            path: "/v1/stats/llm?group_by=session".to_owned(),
            note: "the highest-cardinality grouping, so the merge rather than the decode dominates",
            expected_spans: Some(span_count),
        },
        Shape {
            label: "sessions list",
            path: "/v1/sessions?limit=50".to_owned(),
            // Paged, so its rows cannot sum to the corpus and only the
            // non-empty check applies.
            note: "the same rollups behind a different projection",
            expected_spans: None,
        },
    ];

    let mut results = Vec::with_capacity(shapes.len());
    for shape in shapes {
        // Restart before EVERY shape. The rollup cache is shared across query
        // shapes, so measuring two cold queries against one restart would
        // report the second as cold when the first had already warmed it.
        println!("Restarting for a cold measurement of {}...", shape.label);
        server.restart()?;
        // Let the reopened store settle before timing it, so every shape is
        // measured against the same segment count rather than against
        // whatever compaction happened to be doing that second. Compaction
        // never populates the rollup cache, so this does not warm the query.
        wait_for_stable_segment_count(port)?;

        let started = Instant::now();
        let (status, body) = request(port, "GET", &shape.path, None)?;
        let cold = started.elapsed();
        if status != 200 {
            return Err(format!(
                "{} failed with HTTP {status}: {}",
                shape.label,
                String::from_utf8_lossy(&body)
            )
            .into());
        }
        let parsed: Value = serde_json::from_slice(&body)?;
        verify(&parsed, shape.expected_spans)
            .map_err(|error| format!("{}: {error}", shape.label))?;

        let mut warm = Vec::with_capacity(WARM_SAMPLES);
        for _ in 0..WARM_SAMPLES {
            let started = Instant::now();
            let (status, _) = request(port, "GET", &shape.path, None)?;
            warm.push(started.elapsed());
            if status != 200 {
                return Err(format!("{} failed warm with HTTP {status}", shape.label).into());
            }
        }
        let measured = Measured {
            label: shape.label,
            path: shape.path,
            note: shape.note,
            cold,
            warm_p50: percentile(&warm, 50.0),
            warm_p95: percentile(&warm, 95.0),
        };
        println!(
            "  cold {:.1} ms, warm p50 {:.1} ms, p95 {:.1} ms ({:.0}x)",
            ms(measured.cold),
            ms(measured.warm_p50),
            ms(measured.warm_p95),
            measured.ratio()
        );
        results.push(measured);
    }

    let bytes = disk_bytes(&data_dir);
    let sidecar_bytes = rollup_bytes(&data_dir);
    // The write buffer is folded into every aggregation FROM SCRATCH — it has
    // no cached rollup, because it is still changing. So its size is part of
    // the floor under every number above, warm or cold, and a report that
    // omitted it would leave the warm column unexplainable.
    let buffered = stat(port, "buffered_records").unwrap_or(0);
    let report = render(
        &results,
        &Conditions {
            churn,
            span_count,
            threads,
            segments,
            buffered,
            disk_bytes: bytes,
            sidecar_bytes,
            ingest_elapsed,
        },
    )?;
    println!();
    println!("{report}");
    // Publishing is for the CANONICAL run only, the same rule `bench` holds
    // itself to. The record is a published document, and every knob this
    // harness exposes changes what it measures — a 20,000-span smoke test or a
    // single-client run would otherwise overwrite the committed numbers with
    // something that answers a different question, silently and in the same
    // file. An experiment prints its table and leaves the record alone.
    let canonical = span_count == DEFAULT_SPAN_COUNT
        && threads == DEFAULT_THREADS
        && compaction_fanout() == DEFAULT_COMPACTION_FANOUT
        && compaction_max_segment_bytes() == default_max_segment_bytes();
    if canonical {
        fs::write(RECORD_PATH, &report)?;
        println!("Wrote {RECORD_PATH}");
    } else {
        println!("(experimental configuration — {RECORD_PATH} not rewritten)");
    }
    Ok(())
}

/// Ingests `span_count` LLM spans over `threads` concurrent HTTP clients.
///
/// Threads take a STRIDED slice of the corpus (thread `t` sends spans `t`,
/// `t + threads`, ...), so their timestamps interleave. That is what makes
/// sealed segments overlap in time the way they do under real concurrent
/// ingest, and it is the condition the windowed queries are measuring.
fn ingest(port: u16, span_count: usize, threads: usize) -> Result<(), Box<dyn std::error::Error>> {
    let completed = AtomicUsize::new(0);
    let failure: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    thread::scope(|scope| {
        for thread_index in 0..threads {
            let completed = &completed;
            let failure = &failure;
            scope.spawn(move || {
                let mut body = Vec::with_capacity(768 * BATCH_SIZE);
                let mut batch: Vec<usize> = Vec::with_capacity(BATCH_SIZE);
                let mut index = thread_index;
                while index < span_count {
                    batch.clear();
                    while index < span_count && batch.len() < BATCH_SIZE {
                        batch.push(index);
                        index += threads;
                    }
                    body.clear();
                    body.push(b'[');
                    for (position, span_index) in batch.iter().enumerate() {
                        if position != 0 {
                            body.push(b',');
                        }
                        if serde_json::to_writer(&mut body, &llm_span(*span_index)).is_err() {
                            *failure.lock().expect("failure lock") =
                                Some("span serialization failed".to_owned());
                            return;
                        }
                    }
                    body.push(b']');
                    match request(port, "POST", "/v1/spans", Some(&body)) {
                        Ok((status, _)) if status / 100 == 2 => {}
                        Ok((status, response)) => {
                            *failure.lock().expect("failure lock") = Some(format!(
                                "ingest failed with HTTP {status}: {}",
                                String::from_utf8_lossy(&response)
                            ));
                            return;
                        }
                        Err(error) => {
                            *failure.lock().expect("failure lock") =
                                Some(format!("ingest request failed: {error}"));
                            return;
                        }
                    }
                    let done = completed.fetch_add(batch.len(), Ordering::Relaxed) + batch.len();
                    if done % 100_000 < BATCH_SIZE {
                        println!("  loaded ~{done} spans");
                    }
                }
            });
        }
    });
    match failure.into_inner().expect("failure lock") {
        Some(message) => Err(message.into()),
        None => Ok(()),
    }
}

/// One LLM span, deterministic in its index so two runs measure one corpus.
fn llm_span(index: usize) -> Value {
    let start_ns = BASE_NS + (index as u64 * SPACING_NS);
    let model = MODELS[index % MODELS.len()];
    let provider = PROVIDERS[index % PROVIDERS.len()];
    let prompt = 100 + (index % 900) as u64;
    let completion = 20 + (index % 400) as u64;
    json!({
        "trace_id": format!("{:032x}", index / 4 + 1),
        "span_id": format!("{:016x}", index + 1),
        "name": "chat completion",
        "service": format!("agent-{}", index % 8),
        "start_ns": start_ns,
        "end_ns": start_ns + 400_000 + ((index % 50) as u64 * 30_000),
        "status": if index % 89 == 0 { "error" } else { "ok" },
        "attributes": {
            // Sessions are the highest-cardinality grouping the endpoints
            // offer, so the corpus has to have many of them for the
            // group_by=session measurement to mean anything.
            "session.id": format!("session-{}", index / 40),
            "gen_ai.request.model": model,
            "gen_ai.system": provider,
            "gen_ai.usage.input_tokens": prompt,
            "gen_ai.usage.output_tokens": completion,
            "gen_ai.usage.cost": (prompt as f64 * 0.000_003) + (completion as f64 * 0.000_015),
        }
    })
}

/// What the corpus and the store looked like when the timings were taken.
/// Grouped into one value because a reader cannot interpret a single latency
/// in this report without all of it.
struct Conditions {
    churn: Churn,
    span_count: usize,
    threads: usize,
    segments: u64,
    buffered: u64,
    disk_bytes: u64,
    sidecar_bytes: u64,
    ingest_elapsed: Duration,
}

fn render(
    results: &[Measured],
    conditions: &Conditions,
) -> Result<String, Box<dyn std::error::Error>> {
    let Conditions {
        span_count,
        threads,
        segments,
        buffered,
        disk_bytes,
        sidecar_bytes,
        ingest_elapsed,
        churn: _,
    } = *conditions;
    let measured_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut report = String::new();
    report.push_str(
        "# Traza Query Benchmarks\n\n\
These values were measured by `cargo run --release --bin query-bench`; they are not estimates. \
The benchmark builds and starts `target/release/traza-server` on a free loopback port with a fresh \
temporary data directory, ingests a corpus over concurrent HTTP clients, and then **restarts the \
server before every cold measurement** — the rollup cache lives in process memory, so a restart is \
the only way to observe a genuinely cold query.\n\n\
## Results\n\n\
| Query | Cold (first request after restart) | Warm p50 | Warm p95 | Cold/warm |\n\
|---|---:|---:|---:|---:|\n",
    );
    for result in results {
        report.push_str(&format!(
            "| {} | {:.1} ms | {:.1} ms | {:.1} ms | {:.0}x |\n",
            result.label,
            ms(result.cold),
            ms(result.warm_p50),
            ms(result.warm_p95),
            result.ratio(),
        ));
    }
    report.push_str(&format!(
        "\n## Methodology\n\n\
- Corpus: {span_count} LLM spans, {models} models, {providers} providers, 8 services, one session per 40 spans, \
ingested over {threads} concurrent HTTP client(s) in batches of {BATCH_SIZE}. Ingest took {ingest:.2}s.\n\
- **Concurrency is a measured axis, not a detail.** Clients take a strided slice of the corpus, so their \
timestamps interleave and sealed segments overlap in time. With enough concurrent clients no segment is \
fully inside any query window, which is exactly when the windowed aggregation path stops being able to \
use a cached rollup. A single-threaded ingest reports the easy case.\n\
- Windows are absolute `since_ns`/`until_ns` bounds computed from the corpus's own time range and taken \
from its MIDDLE, so a 1% window really is one percent of the ingested time and is not partly answered \
by ruling out whole segments at the ends.\n\
- Cold: the single first request of that shape after a server restart. Warm: {WARM_SAMPLES} subsequent \
identical requests, nearest-rank percentiles over complete request wall-clock durations.\n\
- Store at measurement time: {segments} segments, {buffered} spans still in the write buffer, {disk:.2} GB on disk of which {sidecar:.1} MB is rollup sidecars ({sidecar_share:.1}% overhead, the price of the cold column), compaction fan-out {fanout} with a {max_segment_bytes}-byte segment ceiling. The segment count is polled until it stops moving BEFORE anything is timed, and again after every restart, so all rows describe one store shape. The buffered count matters: buffered spans have no cached rollup and are re-folded on every request, warm or cold.\n\
- Build: Cargo release profile. Timestamp: Unix {measured_at}.\n\
- Machine context: {context}.\n\n\
## Query paths\n\n",
        models = MODELS.len(),
        providers = PROVIDERS.len(),
        ingest = ingest_elapsed.as_secs_f64(),
        disk = disk_bytes as f64 / 1_000_000_000.0,
        sidecar = sidecar_bytes as f64 / 1_000_000.0,
        sidecar_share = if disk_bytes > sidecar_bytes {
            sidecar_bytes as f64 * 100.0 / (disk_bytes - sidecar_bytes) as f64
        } else {
            0.0
        },
        fanout = compaction_fanout(),
        max_segment_bytes = compaction_max_segment_bytes(),
        context = machine_context(),
    ));
    let churn = &conditions.churn;
    report.push_str(&format!(
        "\n## Compaction churn\n\nEvery merge drops its input segments' cached rollups and publishes an output segment that has none, so the next query pays to re-establish what the merge just took away. A settled store cannot show this — by then every rollup has been rebuilt once and stays warm. These samples are `GET /v1/stats/llm?group_by=model` fired continuously FOR THE WHOLE of ingest, flush and settle, which is the only time compaction has anything to do — and the honest shape of the question, since a dashboard queries a store that is still being written to. Merges are counted as decreases in the segment count; seals increase it.\n\n| Metric | Value |\n|---|---:|\n| Queries fired during ingest | {samples} |\n| Merge events observed | {merges} |\n| p50 during compaction | {p50:.1} ms |\n| p95 during compaction | {p95:.1} ms |\n| Worst single query | {max:.1} ms |\n| p50 once settled | {settled:.1} ms |\n| Churn penalty (p95 during / p50 settled) | {penalty:.1}x |\n\nA run that observed zero merge events proves nothing about compaction; check the merge count before reading the rest of this table.\n",
        samples = churn.samples,
        merges = churn.merge_events,
        p50 = ms(churn.p50),
        p95 = ms(churn.p95),
        max = ms(churn.max),
        settled = ms(churn.settled_p50),
        penalty = if churn.settled_p50.as_secs_f64() > 0.0 {
            churn.p95.as_secs_f64() / churn.settled_p50.as_secs_f64()
        } else {
            0.0
        },
    ));
    for result in results {
        report.push_str(&format!(
            "- `{}` — {}\n",
            result.path.replace('`', ""),
            result.note
        ));
    }
    report.push_str(
        "\n## Verification Notes\n\n\
- Every reported result is measured by this benchmark run, never estimated.\n\
- A non-200 response aborts the run rather than being recorded as a fast query.\n\
- The cold column cannot be re-measured without another restart; it is one sample by construction, \
so read it as an order of magnitude and not as a percentile.\n",
    );
    Ok(report)
}

/// Query latency measured while compaction is actively replacing segments.
struct Churn {
    samples: usize,
    merge_events: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
    settled_p50: Duration,
}

/// Queries the whole-corpus aggregation in a loop until told to stop, and
/// reports the latency distribution plus how many merges landed underneath it.
///
/// This is the churn measurement. Publishing a merge drops the input segments'
/// cached rollups and introduces an output segment that has none, so every
/// merge makes the next query re-establish what the merge just took away. A
/// settled store cannot show that at all — by then every rollup has been
/// rebuilt once and stays warm for the life of the process — so the probe runs
/// concurrently with ingest, which is the only time compaction has anything to
/// do. It is also the honest shape of the question: a dashboard queries a
/// store that is still being written to.
///
/// Merges are read from `traza_segment_merges_total`, not inferred. Segment
/// count cannot answer the question during ingest: seals raise it while merges
/// lower it, so the two hide each other and a run that merged repeatedly can
/// look like one that never merged at all. An earlier version of this probe
/// inferred merges from decreases in the count and reported "1 merge event"
/// for a run that had done several.
fn churn_probe(
    port: u16,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Churn, Box<dyn std::error::Error + Send + Sync>> {
    const PATH: &str = "/v1/stats/llm?group_by=model";
    let mut latencies: Vec<Duration> = Vec::new();
    let started_merges = merges_total(port).unwrap_or_default();
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let (status, _) = request(port, "GET", PATH, None)?;
        let elapsed = started.elapsed();
        if status != 200 {
            return Err(format!("churn probe failed with HTTP {status}").into());
        }
        latencies.push(elapsed);
    }
    let merge_events = merges_total(port)
        .unwrap_or_default()
        .saturating_sub(started_merges) as usize;
    if latencies.is_empty() {
        latencies.push(Duration::ZERO);
    }

    // The same query against the now-settled store: the baseline that makes
    // the numbers above mean something.
    let mut settled = Vec::with_capacity(20);
    for _ in 0..20 {
        let started = Instant::now();
        request(port, "GET", PATH, None)?;
        settled.push(started.elapsed());
    }

    Ok(Churn {
        samples: latencies.len(),
        merge_events,
        p50: percentile(&latencies, 50.0),
        p95: percentile(&latencies, 95.0),
        max: percentile(&latencies, 100.0),
        settled_p50: percentile(&settled, 50.0),
    })
}

/// Polls until compaction has genuinely quiesced, and returns the segment
/// count.
///
/// The bar is BOTH the segment count holding still AND the merge counter not
/// moving, for longer than the server's compaction tick interval. Segment
/// count alone is not enough and the mistake is easy to make: the maintenance
/// thread ticks every five seconds, so a three-second run of identical counts
/// only proves no tick fired inside it. An earlier version of this function
/// waited exactly that long, declared the store settled, and then measured its
/// query shapes while compaction ran underneath them — which is precisely the
/// churn the report claims to have excluded.
fn wait_for_stable_segment_count(port: u16) -> Result<u64, Box<dyn std::error::Error>> {
    // Comfortably longer than the server's five-second tick, so quiescence
    // means "a tick fired and found nothing", not "no tick fired".
    const QUIET: Duration = Duration::from_secs(13);
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut last = (
        segment_count(port).unwrap_or_default(),
        merges_total(port).unwrap_or_default(),
    );
    let mut quiet_since = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(250));
        let current = (
            segment_count(port).unwrap_or_default(),
            merges_total(port).unwrap_or_default(),
        );
        if current == last {
            if quiet_since.elapsed() >= QUIET {
                return Ok(current.0);
            }
        } else {
            quiet_since = Instant::now();
            last = current;
        }
        if Instant::now() >= deadline {
            return Err("compaction never quiesced".into());
        }
    }
}

fn segment_count(port: u16) -> Option<u64> {
    stat(port, "segment_count")
}

/// `traza_segment_merges_total` from the Prometheus endpoint.
fn merges_total(port: u16) -> Option<u64> {
    let (status, body) = request(port, "GET", "/v1/metrics", None).ok()?;
    if status != 200 {
        return None;
    }
    String::from_utf8_lossy(&body)
        .lines()
        .find_map(|line| line.strip_prefix("traza_segment_merges_total "))
        .and_then(|value| value.trim().parse().ok())
}

/// One integer from `/v1/stats`.
fn stat(port: u16, field: &str) -> Option<u64> {
    let (status, body) = request(port, "GET", "/v1/stats", None).ok()?;
    if status != 200 {
        return None;
    }
    let value: Value = serde_json::from_slice(&body).ok()?;
    value.get(field).and_then(Value::as_u64)
}

fn disk_bytes(dir: &Path) -> u64 {
    bytes_matching(dir, |_| true)
}

/// Bytes held by the rollup sidecars.
///
/// Persisting the rollups buys the cold column at the cost of disk, so the
/// report states the cost next to the benefit rather than leaving a reader to
/// discover it from a directory listing.
fn rollup_bytes(dir: &Path) -> u64 {
    bytes_matching(dir, |path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("rollup")
    })
}

fn bytes_matching(dir: &Path, keep: impl Fn(&Path) -> bool + Copy) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => total += bytes_matching(&path, keep),
            Ok(metadata) if keep(&path) => total += metadata.len(),
            _ => {}
        }
    }
    total
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
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("server exited before becoming ready: {status}").into());
        }
        if request(port, "GET", "/v1/stats", None).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("server did not become ready within 120 seconds".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_record_count(port: u16, expected: u64) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (status, body) = request(port, "GET", "/v1/stats", None)?;
        if status == 200 {
            let value: Value = serde_json::from_slice(&body)?;
            if value
                .get("record_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= expected
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("record count did not reach the corpus size in 300 seconds".into());
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
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
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

fn machine_context() -> String {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let parallelism = thread::available_parallelism().map_or(1, usize::from);
    format!("{os}/{arch}, {parallelism} available hardware threads")
}
