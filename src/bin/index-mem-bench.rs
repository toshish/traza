//! What does a segment's resident attribute index actually cost?
//!
//! The store's stated memory rule was "O(indexes), not O(data)": segments are
//! file-backed, only their parsed indexes stay resident, payloads are read on
//! demand. Every clause of that is true. What it does not say is how large
//! "indexes" gets, and for LLM traffic that is the whole question, because
//! `Segment` keys its attribute index on the **entire attribute value**:
//!
//! ```text
//! attribute_index: HashMap<(String, String), Vec<u64>>
//! ```
//!
//! Every distinct prompt or completion short enough to stay inline is
//! therefore pinned in RAM, in full, for the life of the segment. This
//! harness measures that, and it is built around three ways the measurement
//! could quietly lie:
//!
//! - **`Store::resident_payload_bytes()` reports zero by design.** It counts
//!   the payload encoding and deliberately excludes indexes, so measuring
//!   with it "proves" there is no problem. The numbers here are process RSS
//!   read from the OS, alongside the approximate `resident_index_bytes()`
//!   diagnostic and the segment headers' own `attribute_index_len`.
//! - **A borrowed corpus controls nothing.** Neither existing generator lets
//!   you vary the axis under test. `bench.rs` emits five enum-valued
//!   attributes, which is the cheap case by construction and the case every
//!   recorded RSS figure came from; `seed.rs` is closer to real LLM traffic
//!   but its cardinality is whatever it happens to be, and its deliberately
//!   oversized values cross the offload threshold and leave the index
//!   entirely. This harness generates its own corpus with value size,
//!   uniqueness, locality and attribute count as explicit axes, and asserts
//!   that nothing was offloaded — an offloaded value is not measured at all,
//!   and would report a reassuring number about nothing.
//! - **A dirty process hides the steady state.** After ingest the allocator
//!   still holds the write buffer's pages. The reopen and compaction
//!   measurements therefore run in a **fresh child process** that does
//!   nothing but open the store, which is the cleanest available signal of
//!   pure index residency.
//!
//! Durability is [`Durability::Buffered`] throughout, on purpose: a WAL would
//! replay into the write buffer on reopen and put span text back in RAM by a
//! path that has nothing to do with segment indexes. The question is what an
//! *opened segment set* costs.
//!
//! ```text
//! cargo run --release --bin index-mem-bench -- --matrix
//! cargo run --release --bin index-mem-bench -- --value-bytes 2048 --unique-pct 100
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde_json::{json, Value};
use traza::{segment, CompactionConfig, Config, Durability, Span, Store};

/// Default inline-offload threshold: the `traza-server` default. Values above
/// it leave the attribute index entirely, which is exactly the case this
/// harness must avoid measuring by accident.
const DEFAULT_PAYLOAD_THRESHOLD: usize = 256 * 1024;

/// Bytes of attribute text ingested per cell, per text attribute. Cells vary
/// the value size, so holding the text volume fixed is what makes "resident
/// bytes per GB of ingested text" comparable across the matrix.
const DEFAULT_TEXT_BUDGET: usize = 256 * 1024 * 1024;

/// Spans handed to `ingest_batch` at a time. Large enough to amortize the
/// writer lock, small enough that the batch itself is not the peak.
const BATCH_SPANS: usize = 256;

const PROMPT_KEY: &str = "gen_ai.prompt";
const COMPLETION_KEY: &str = "gen_ai.completion";

/// Shared template body. Prose-shaped rather than random: random bytes are
/// unrealistic for prompts and, more importantly, behave differently — they
/// defeat every form of similarity the design might otherwise exploit, so a
/// result measured on them would not transfer.
const FILLER: &str = "You are a careful assistant. Read the attached context and answer the \
                      question using only what it contains. Cite the paragraph you relied on \
                      for every claim, and if the context does not settle the question, say \
                      so plainly instead of guessing. Context follows. ";

// ------------------------------------------------------------------ options

/// How distinct values are distributed across the span sequence, which
/// decides how many SEGMENTS each distinct value lands in. A value present in
/// two segments is decoded and retained twice, so this is not cosmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// `value = i % distinct`: consecutive spans get different values and
    /// each distinct value recurs throughout the run, so it appears in every
    /// segment. The shape of a system prompt reused over hours.
    Interleaved,
    /// Distinct values arrive in contiguous blocks, so each lands in one
    /// segment. The best case for residency.
    Blocked,
}

impl Order {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interleaved => "interleaved",
            Self::Blocked => "blocked",
        }
    }
}

/// One point in the corpus matrix.
#[derive(Clone, Copy, Debug)]
struct Cell {
    /// Bytes per inline attribute value. Must stay below the offload
    /// threshold or the value never reaches the index.
    value_bytes: usize,
    /// Percentage of spans carrying a distinct value.
    unique_pct: u32,
    /// Indexed text attributes per span: 1 (prompt) or 2 (prompt+completion).
    attrs: usize,
    /// Extra low-cardinality enum-valued attributes per span, the shape of
    /// conventional tracing (`http.method`, `db.system`, a deployment tier).
    /// These cost almost no value bytes and one 8-byte posting per span, so
    /// they isolate the floor the design cannot get below.
    enum_attrs: usize,
    /// `Config::flush_spans`, which sets the segment size.
    flush_spans: usize,
    order: Order,
    spans: usize,
}

impl Cell {
    fn distinct(&self) -> usize {
        let count = self.spans * self.unique_pct as usize / 100;
        count.max(1)
    }

    fn label(&self) -> String {
        let enums = if self.enum_attrs > 0 {
            format!("+{}enum", self.enum_attrs)
        } else {
            String::new()
        };
        format!(
            "{}B/{}%/{}attr{}/flush{}/{}",
            self.value_bytes,
            self.unique_pct,
            self.attrs,
            enums,
            self.flush_spans,
            self.order.as_str()
        )
    }
}

// ------------------------------------------------------------------- corpus

/// A templated value of exactly `size` ASCII bytes whose unique part is a
/// short identifier header — the realistic shape, where nearly every byte is
/// shared boilerplate and only a few differ. It is still a distinct `String`
/// in the index, which is the point being measured.
fn templated_value(kind: &str, id: u64, size: usize) -> String {
    let mut out = String::with_capacity(size + FILLER.len());
    out.push_str(&format!(
        "[{kind} req {id:012} tenant t{:04} session s{:08}] ",
        id % 97,
        id.wrapping_mul(2_654_435_761) % 100_000_000
    ));
    while out.len() < size {
        out.push_str(FILLER);
    }
    // Every byte written above is ASCII, so this cannot split a character.
    out.truncate(size);
    out
}

fn value_id(index: usize, cell: &Cell) -> u64 {
    let distinct = cell.distinct() as u64;
    match cell.order {
        Order::Interleaved => index as u64 % distinct,
        Order::Blocked => (index as u64).saturating_mul(distinct) / cell.spans.max(1) as u64,
    }
}

fn make_span(index: usize, cell: &Cell) -> Span {
    let id = value_id(index, cell);
    let mut attributes = serde_json::Map::new();
    attributes.insert(
        PROMPT_KEY.to_owned(),
        Value::String(templated_value("prompt", id, cell.value_bytes)),
    );
    if cell.attrs >= 2 {
        attributes.insert(
            COMPLETION_KEY.to_owned(),
            Value::String(templated_value("completion", id, cell.value_bytes)),
        );
    }
    for extra in 0..cell.enum_attrs {
        // Distinct value counts chosen to be enum-shaped and to differ from
        // each other, so no single one dominates the breakdown.
        let cardinality = 2 + extra * 24;
        attributes.insert(
            format!("deployment.dimension{extra}"),
            Value::String(format!("v{}", index % (cardinality + 1))),
        );
    }
    let start = 1_700_000_000_000_000_000 + index as u64 * 1_000_000;
    Span {
        trace_id: format!("trace-{:012}", index / 8),
        span_id: format!("span-{index:012}"),
        parent_span_id: None,
        name: "chat.completion".to_owned(),
        start_time_ns: start,
        end_time_ns: start + 250_000_000,
        status: "OK".to_owned(),
        service: "agent-runtime".to_owned(),
        attributes,
        events: Vec::new(),
        links: Vec::new(),
        extra: serde_json::Map::new(),
    }
}

// -------------------------------------------------------------- measurement

/// Process resident set size in bytes, read from the OS.
///
/// `ps -o rss=` reports KiB on both macOS and Linux and needs no dependency.
/// It is the *current* RSS, not the peak; the peak sampler below covers that.
fn rss_bytes() -> u64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(0, |kib| kib * 1024)
}

fn load_average() -> String {
    Command::new("uptime")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| {
            text.split("load average")
                .nth(1)
                .map(|tail| tail.trim_start_matches([':', 's', ' ']).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn machine() -> String {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let model = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown CPU".to_owned());
    format!(
        "{}/{}, {threads} hardware threads, {model}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Summed on-disk section lengths across every segment file in `directory`,
/// read straight from the headers. This is the serialized size of what
/// `Segment::open` decodes, and it is an independent check on the in-memory
/// estimate: the resident form cannot be smaller than the bytes it decodes.
fn on_disk(directory: &Path) -> (u64, u64, usize, u64) {
    let mut attribute_index = 0;
    let mut total = 0;
    let mut segments = 0;
    let mut records = 0;
    let Ok(entries) = fs::read_dir(directory) else {
        return (0, 0, 0, 0);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let Ok(opened) = segment::Segment::open(&path) else {
            continue;
        };
        attribute_index += opened.header().attribute_index_len;
        total += metadata.len();
        records += opened.header().record_count;
        segments += 1;
    }
    (attribute_index, total, segments, records)
}

/// Resident attribute-index cost per attribute key, summed over every segment
/// file in `directory`.
///
/// Reads the segment files directly rather than going through the store: the
/// question is what the on-disk set costs to open, and answering it from the
/// files keeps the engine's own segment list out of the measurement.
fn cost_by_key(directory: &Path) -> Value {
    let mut totals: std::collections::BTreeMap<String, (usize, usize)> =
        std::collections::BTreeMap::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return json!({});
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
            continue;
        }
        let Ok(opened) = segment::Segment::open(&path) else {
            continue;
        };
        for (key, (values, bytes)) in opened.attribute_index_cost_by_key() {
            let total = totals.entry(key).or_insert((0, 0));
            total.0 += values;
            total.1 += bytes;
        }
    }
    let mut report = serde_json::Map::new();
    for (key, (values, bytes)) in totals {
        report.insert(key, json!({ "values": values, "bytes": bytes }));
    }
    Value::Object(report)
}

fn payload_file_count(directory: &Path) -> usize {
    fn walk(path: &Path, count: &mut usize) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                walk(&child, count);
            } else {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(&directory.join("payloads"), &mut count);
    count
}

fn store_config(flush_spans: usize, payload_threshold: usize) -> Config {
    Config {
        flush_spans,
        // Buffered on purpose: see the module docs. A replayed log would put
        // span text back in the write buffer and contaminate the reopen
        // measurement with something that is not a segment index.
        durability: Durability::Buffered,
        payload_threshold: (payload_threshold > 0).then_some(payload_threshold),
        compaction: Some(CompactionConfig::default()),
        ..Config::default()
    }
}

// ------------------------------------------------------------- child probes

/// Opens the store in this otherwise-empty process and reports what that
/// costs. Optionally compacts first and reports the cost afterwards.
///
/// Running this as its own process is the measurement: after an ingest the
/// parent's allocator still holds the write buffer's pages, so its RSS
/// answers "how much did this process ever need", not "how much does serving
/// this store cost". A fresh process answers the second question.
fn probe(directory: &Path, flush_spans: usize, payload_threshold: usize, compact: bool) {
    let rss_start = rss_bytes();
    let store = Store::open(directory, store_config(flush_spans, payload_threshold))
        .expect("probe: open store");
    let rss_open = rss_bytes();
    let index_bytes = store.resident_index_bytes().expect("probe: index bytes");
    let entries = store
        .resident_attribute_index_entries()
        .expect("probe: index entries");
    let payload_bytes = store
        .resident_payload_bytes()
        .expect("probe: payload bytes");
    let stats = store.stats().expect("probe: stats");

    let mut report = json!({
        "rss_start": rss_start,
        "rss_open": rss_open,
        "resident_index_bytes": index_bytes,
        "resident_attribute_index_entries": entries,
        "resident_payload_bytes": payload_bytes,
        "segments": stats.segment_count,
        "records": stats.total_records,
        "by_attribute_key": cost_by_key(directory),
    });

    if compact {
        let started = Instant::now();
        let merged = store.compact_segments().expect("probe: compact");
        let rss_compacted = rss_bytes();
        let after = store.resident_index_bytes().expect("probe: index bytes");
        let stats = store.stats().expect("probe: stats");
        report["compacted_away"] = json!(merged);
        report["compact_seconds"] = json!(started.elapsed().as_secs_f64());
        report["rss_compacted"] = json!(rss_compacted);
        report["resident_index_bytes_compacted"] = json!(after);
        report["segments_compacted"] = json!(stats.segment_count);
    }

    println!("{report}");
}

fn run_probe(exe: &Path, directory: &Path, cell: &Cell, threshold: usize, compact: bool) -> Value {
    let mut command = Command::new(exe);
    command
        .arg("--probe")
        .arg(directory)
        .arg("--flush-spans")
        .arg(cell.flush_spans.to_string())
        .arg("--payload-threshold")
        .arg(threshold.to_string());
    if compact {
        command.arg("--compact");
    }
    let output = command.output().expect("spawn probe");
    if !output.status.success() {
        eprintln!("probe failed: {}", String::from_utf8_lossy(&output.stderr));
        return json!({});
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|_| json!({}))
}

// --------------------------------------------------------------- the cell

fn run_cell(exe: &Path, root: &Path, cell: &Cell, threshold: usize, keep: bool) -> Value {
    let directory = root.join(format!("cell-{}", cell.label().replace(['/', '%'], "-")));
    let _ = fs::remove_dir_all(&directory);

    let rss_base = rss_bytes();
    let started = Instant::now();
    let text_bytes = {
        let store =
            Store::open(&directory, store_config(cell.flush_spans, threshold)).expect("open store");
        let mut text_bytes = 0u64;
        let mut batch = Vec::with_capacity(BATCH_SPANS);
        for index in 0..cell.spans {
            let span = make_span(index, cell);
            text_bytes += (cell.value_bytes * cell.attrs) as u64;
            batch.push(span);
            if batch.len() == BATCH_SPANS {
                store
                    .ingest_batch(std::mem::take(&mut batch))
                    .expect("ingest");
                batch.reserve(BATCH_SPANS);
            }
        }
        if !batch.is_empty() {
            store.ingest_batch(batch).expect("ingest");
        }
        store.flush().expect("flush");
        text_bytes
    };
    let ingest_seconds = started.elapsed().as_secs_f64();
    let rss_after_ingest = rss_bytes();

    let (attribute_index_len, disk_bytes, segments, records) = on_disk(&directory);
    let offloaded = payload_file_count(&directory);

    let reopen = run_probe(exe, &directory, cell, threshold, false);
    let compacted = run_probe(exe, &directory, cell, threshold, true);
    if !keep {
        let _ = fs::remove_dir_all(&directory);
    }

    json!({
        "label": cell.label(),
        "value_bytes": cell.value_bytes,
        "unique_pct": cell.unique_pct,
        "attrs": cell.attrs,
        "flush_spans": cell.flush_spans,
        "order": cell.order.as_str(),
        "spans": cell.spans,
        "distinct_values": cell.distinct(),
        "ingested_text_bytes": text_bytes,
        "ingest_seconds": ingest_seconds,
        "rss_base": rss_base,
        "rss_after_ingest": rss_after_ingest,
        "segments": segments,
        "records": records,
        "disk_bytes": disk_bytes,
        "on_disk_attribute_index_bytes": attribute_index_len,
        // Non-zero here would mean values were offloaded to the payload store
        // and never entered the attribute index — the trap that turns this
        // whole benchmark into a reassuring number about nothing.
        "offloaded_payload_files": offloaded,
        "load_average": load_average(),
        "reopen": reopen,
        "compacted": compacted,
    })
}

// --------------------------------------------------------------- the matrix

fn matrix(text_budget: usize) -> Vec<Cell> {
    let spans_for = |value_bytes: usize| (text_budget / value_bytes).max(1_000);
    let mut cells = Vec::new();

    // Core arm: value size against uniqueness, one attribute, default
    // segment size. This is the arm that answers the question.
    for value_bytes in [512, 2 * 1024, 8 * 1024, 64 * 1024] {
        for unique_pct in [0, 10, 50, 100] {
            cells.push(Cell {
                value_bytes,
                unique_pct,
                attrs: 1,
                enum_attrs: 0,
                flush_spans: 10_000,
                order: Order::Interleaved,
                spans: spans_for(value_bytes),
            });
        }
    }

    // Locality arm: the same partial-uniqueness cells with each distinct
    // value confined to one segment. The gap between the two orders is the
    // cost of cross-segment duplication, which no per-segment reasoning about
    // cardinality would predict.
    for value_bytes in [2 * 1024, 8 * 1024] {
        for unique_pct in [10, 50] {
            cells.push(Cell {
                value_bytes,
                unique_pct,
                attrs: 1,
                enum_attrs: 0,
                flush_spans: 10_000,
                order: Order::Blocked,
                spans: spans_for(value_bytes),
            });
        }
    }

    // Attribute-count arm: prompt alone against prompt plus completion.
    for value_bytes in [2 * 1024, 8 * 1024] {
        cells.push(Cell {
            value_bytes,
            unique_pct: 100,
            attrs: 2,
            enum_attrs: 0,
            flush_spans: 10_000,
            order: Order::Interleaved,
            spans: spans_for(value_bytes),
        });
    }

    // Segment-size arm: the `latency` profile, the default, and `throughput`.
    for flush_spans in [5_000, 30_000] {
        cells.push(Cell {
            value_bytes: 2 * 1024,
            unique_pct: 100,
            attrs: 1,
            enum_attrs: 0,
            flush_spans,
            order: Order::Interleaved,
            spans: spans_for(2 * 1024),
        });
    }

    // Conventional-tracing arm: one repeated value plus enum-valued
    // attributes. This is the corpus the original claim was measured on, and
    // it is where the claim still holds; the arm exists so the correction
    // does not overshoot into saying the design is bad everywhere.
    cells.push(Cell {
        value_bytes: 512,
        unique_pct: 0,
        attrs: 1,
        enum_attrs: 3,
        flush_spans: 10_000,
        order: Order::Interleaved,
        spans: spans_for(512),
    });

    // Control for the 64 KiB core cells. At a fixed text budget those fit in
    // ONE segment, so their RSS carries the full transient of decoding a
    // single large index section in one go. Splitting the same corpus across
    // segments separates that transient from the steady state.
    cells.push(Cell {
        value_bytes: 64 * 1024,
        unique_pct: 100,
        attrs: 1,
        enum_attrs: 0,
        flush_spans: 1_000,
        order: Order::Interleaved,
        spans: spans_for(64 * 1024),
    });

    cells
}

// ---------------------------------------------------------------- reporting

fn mib(bytes: f64) -> String {
    format!("{:.1}", bytes / (1024.0 * 1024.0))
}

fn number(value: &Value, path: &[&str]) -> f64 {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    current.as_f64().unwrap_or(0.0)
}

fn report(results: &[Value]) -> String {
    let mut out = String::new();
    out.push_str("\n| cell | spans | distinct | text MiB | disk MiB | idx MiB (disk) | ");
    out.push_str("approx idx MiB | RSS MiB (reopen) | RSS MiB (compacted) | resident/GB text |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for result in results {
        let text = number(result, &["ingested_text_bytes"]);
        let reopen_rss = number(result, &["reopen", "rss_open"]);
        let reopen_start = number(result, &["reopen", "rss_start"]);
        let index = number(result, &["reopen", "resident_index_bytes"]);
        let compacted_rss = number(result, &["compacted", "rss_compacted"]);
        let per_gb = if text > 0.0 {
            (reopen_rss - reopen_start) * (1024.0 * 1024.0 * 1024.0) / text
        } else {
            0.0
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} MiB |\n",
            result["label"].as_str().unwrap_or("?"),
            number(result, &["spans"]),
            number(result, &["distinct_values"]),
            mib(text),
            mib(number(result, &["disk_bytes"])),
            mib(number(result, &["on_disk_attribute_index_bytes"])),
            mib(index),
            mib(reopen_rss),
            mib(compacted_rss),
            mib(per_gb),
        ));
    }
    out
}

// -------------------------------------------------------------------- main

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let exe = std::env::current_exe().expect("current exe");

    let mut probe_dir: Option<PathBuf> = None;
    let mut compact = false;
    let mut run_matrix = false;
    let mut value_bytes = 2 * 1024_usize;
    let mut unique_pct = 100_u32;
    let mut attrs = 1_usize;
    let mut enum_attrs = 0_usize;
    let mut keep = false;
    let mut allow_offload = false;
    let mut flush_spans = 10_000_usize;
    let mut spans: Option<usize> = None;
    let mut order = Order::Interleaved;
    let mut threshold = DEFAULT_PAYLOAD_THRESHOLD;
    let mut text_budget = DEFAULT_TEXT_BUDGET;
    let mut root = std::env::temp_dir().join("traza-index-mem-bench");

    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let mut next = |what: &str| -> String {
            index += 1;
            arguments
                .get(index)
                .cloned()
                .unwrap_or_else(|| panic!("{what} needs a value"))
        };
        match argument {
            "--probe" => probe_dir = Some(PathBuf::from(next("--probe"))),
            "--compact" => compact = true,
            "--matrix" => run_matrix = true,
            "--value-bytes" => value_bytes = next("--value-bytes").parse().expect("integer"),
            "--unique-pct" => unique_pct = next("--unique-pct").parse().expect("integer"),
            "--attrs" => attrs = next("--attrs").parse().expect("integer"),
            "--enum-attrs" => enum_attrs = next("--enum-attrs").parse().expect("integer"),
            "--keep" => keep = true,
            // Deliberately measuring the offload path: values LEAVE the index
            // and are replaced by a reference object, which is the one lever
            // an operator has against a large resident index.
            "--allow-offload" => allow_offload = true,
            "--flush-spans" => flush_spans = next("--flush-spans").parse().expect("integer"),
            "--spans" => spans = Some(next("--spans").parse().expect("integer")),
            "--blocked" => order = Order::Blocked,
            "--payload-threshold" => {
                threshold = next("--payload-threshold").parse().expect("integer");
            }
            "--text-budget-mib" => {
                text_budget =
                    next("--text-budget-mib").parse::<usize>().expect("integer") * 1024 * 1024;
            }
            "--root" => root = PathBuf::from(next("--root")),
            "--help" | "-h" => {
                println!("{}", usage());
                return;
            }
            other => panic!("unknown argument: {other}"),
        }
        index += 1;
    }

    if let Some(directory) = probe_dir {
        probe(&directory, flush_spans, threshold, compact);
        return;
    }

    // A value at or above the offload threshold never reaches the attribute
    // index, so a matrix that quietly crossed it would measure nothing and
    // report a comfortable number for it.
    assert!(
        allow_offload || threshold == 0 || value_bytes < threshold,
        "value_bytes {value_bytes} is not below the offload threshold {threshold}: \
         such values are moved to the payload store and never indexed"
    );

    fs::create_dir_all(&root).expect("create root");
    let cells = if run_matrix {
        matrix(text_budget)
    } else {
        vec![Cell {
            value_bytes,
            unique_pct,
            attrs,
            enum_attrs,
            flush_spans,
            order,
            spans: spans.unwrap_or_else(|| (text_budget / value_bytes).max(1_000)),
        }]
    };

    println!("machine: {}", machine());
    println!("load average at start: {}", load_average());
    println!("payload offload threshold: {threshold} bytes");
    println!("cells: {}", cells.len());

    let mut results = Vec::new();
    for cell in &cells {
        eprintln!("running {} ...", cell.label());
        let result = run_cell(&exe, &root, cell, threshold, keep);
        let offloaded = number(&result, &["offloaded_payload_files"]);
        assert!(
            allow_offload || offloaded == 0.0,
            "cell {} offloaded {offloaded} payload files: its values left the \
             attribute index and the measurement is void",
            cell.label()
        );
        println!("{result}");
        results.push(result);
    }

    println!("{}", report(&results));
    println!("load average at end: {}", load_average());
}

fn usage() -> String {
    "index-mem-bench — measure what a segment's resident attribute index costs\n\
     \n\
     --matrix                 run the whole corpus matrix\n\
     --value-bytes N          inline attribute value size (default 2048)\n\
     --unique-pct P           percentage of spans with a distinct value (default 100)\n\
     --attrs N                indexed text attributes per span, 1 or 2 (default 1)\n\
     --enum-attrs N           extra low-cardinality indexed attributes (default 0)\n\
     --keep                   leave each cell's data directory in place\n\
     --allow-offload          permit values above the threshold (measures offloading)\n\
     --flush-spans N          segment size (default 10000)\n\
     --spans N                span count (default: text budget / value size)\n\
     --blocked                confine each distinct value to one segment\n\
     --text-budget-mib N      attribute text per cell, per attribute (default 128)\n\
     --payload-threshold N    inline offload threshold (default 262144, 0 disables)\n\
     --root DIR               where cell data directories are created\n\
     --probe DIR [--compact]  internal: measure a reopen in a fresh process\n"
        .to_owned()
}
