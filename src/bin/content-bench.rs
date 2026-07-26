//! What does content search cost, and what does the index buy?
//!
//! Content search is a feature whose value is entirely a performance claim:
//! the same rows come back either way, because a segment without a content
//! index is scanned rather than skipped. So "it works" proves nothing, and
//! this harness exists to make the difference measurable rather than asserted.
//!
//! Every cell is run **twice over identical corpora** — once with
//! `content_index: true` and once with it off — so the speedup is a
//! difference between two measurements rather than a comparison against a
//! remembered number. The corpora are generated from the same seed, so they
//! are the same bytes.
//!
//! Three things are reported, because a partial answer here is misleading:
//!
//! - **Query latency**, split by how selective the term is. A word in one
//!   span and a word in every span exercise opposite ends of the index, and
//!   quoting only the first would be advertising.
//! - **What it costs to build**: seal-time CPU shows up as ingest wall clock,
//!   and the index shows up as segment bytes.
//! - **What it costs to hold**: the resident summary filter, which is the
//!   only part that stays in RAM, and its fill ratio — a saturated filter
//!   still answers correctly and has stopped pruning, and nothing else would
//!   reveal that.
//!
//! ```text
//! cargo run --release --bin content-bench
//! cargo run --release --bin content-bench -- --spans 200000 --repeats 5
//! ```

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use traza::{Config, Durability, Span, SpanFilter, Store};

/// Vocabulary the synthetic prose is drawn from. Small enough that common
/// words really are common, which is what makes the unselective case honest.
const COMMON: &[&str] = &[
    "the",
    "user",
    "asked",
    "about",
    "their",
    "order",
    "status",
    "and",
    "the",
    "assistant",
    "replied",
    "with",
    "a",
    "summary",
    "of",
    "recent",
    "activity",
    "including",
    "shipping",
    "details",
    "payment",
    "method",
    "and",
    "estimated",
    "delivery",
    "window",
    "for",
    "each",
    "item",
    "in",
    "the",
    "basket",
];

/// A deterministic generator, so both halves of a comparison see identical
/// bytes. Reusing `rand` is not an option — this crate takes no dependencies.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next() >> 33) as usize % upper
    }
}

/// One span's worth of LLM-shaped text: a prompt and a completion of ordinary
/// words, plus a per-span nonce word so that vocabulary grows with the corpus
/// the way real text does rather than saturating at 32 words.
fn text(rng: &mut Lcg, index: usize, words: usize) -> (String, String) {
    let mut prompt = String::with_capacity(words * 8);
    for _ in 0..words {
        prompt.push_str(COMMON[rng.below(COMMON.len())]);
        prompt.push(' ');
    }
    prompt.push_str(&format!("ticket{index}"));
    let mut completion = String::with_capacity(words * 8);
    for _ in 0..words {
        completion.push_str(COMMON[rng.below(COMMON.len())]);
        completion.push(' ');
    }
    // One span in the corpus, and only one, carries the rare term.
    if index == 0 {
        completion.push_str("antidisestablishmentarianism");
    }
    (prompt, completion)
}

fn span(index: usize, prompt: String, completion: String) -> Span {
    serde_json::from_value(json!({
        "trace_id": format!("t{}", index / 4),
        "span_id": format!("s{index}"),
        "name": "chat",
        "service": "agent",
        "start_time_ns": 1_700_000_000_000_000_000_u64 + index as u64 * 1_000,
        "end_time_ns": 1_700_000_000_000_000_000_u64 + index as u64 * 1_000 + 500,
        "attributes": {
            "gen_ai.prompt": prompt,
            "gen_ai.completion": completion,
            "gen_ai.request.model": "gpt-4o",
        },
    }))
    .expect("span")
}

struct Cell {
    ingest: Duration,
    disk_bytes: u64,
    segments: usize,
    resident_index_bytes: usize,
    content_resident_bytes: usize,
    summary_fill: Option<f64>,
    text_bytes: u64,
    latencies: Vec<(&'static str, Duration, usize)>,
}

/// The queries, chosen to span the selectivity range rather than to flatter.
const QUERIES: [(&str, &str); 4] = [
    // In exactly one span of the corpus.
    ("rare", "antidisestablishmentarianism"),
    // In exactly one span, but a word the tokenizer sees everywhere else too.
    ("selective_pair", "ticket7 shipping"),
    // In essentially every span: the index cannot help, and must not pretend.
    ("common", "shipping"),
    // In no span at all: the best case, answered from resident filters alone.
    ("absent", "zygomorphic"),
];

fn run(directory: &Path, spans: usize, words: usize, content_index: bool, repeats: usize) -> Cell {
    let _ = std::fs::remove_dir_all(directory);
    std::fs::create_dir_all(directory).expect("create data directory");
    let store = Store::open(
        directory,
        Config {
            // Buffered on purpose: a WAL would replay into the write buffer
            // and put span text back in RAM by a path that has nothing to do
            // with segment indexes. The question is what a SEGMENT SET costs.
            durability: Durability::Buffered,
            // Off, so segment count is a function of the corpus rather than
            // of when a merge happened to run.
            compaction: None,
            flush_spans: 2_000,
            payload_threshold: None,
            content_index,
            ..Config::default()
        },
    )
    .expect("opens");

    let mut rng = Lcg(0x5eed_1234_5678_9abc);
    let mut text_bytes = 0u64;
    let started = Instant::now();
    let mut batch = Vec::with_capacity(1_000);
    for index in 0..spans {
        let (prompt, completion) = text(&mut rng, index, words);
        text_bytes += (prompt.len() + completion.len()) as u64;
        batch.push(span(index, prompt, completion));
        if batch.len() == 1_000 {
            store
                .ingest_batch(std::mem::take(&mut batch))
                .expect("ingest");
            batch.reserve(1_000);
        }
    }
    if !batch.is_empty() {
        store.ingest_batch(batch).expect("ingest");
    }
    store.flush().expect("flush");
    let ingest = started.elapsed();

    let stats = store.stats().expect("stats");
    let mut latencies = Vec::new();
    for (label, needle) in QUERIES {
        let filter = SpanFilter {
            content: Some(needle.to_owned()),
            ..SpanFilter::default()
        };
        // One warm run first, so the comparison is not measuring page-cache
        // misses on one side only. Then take the BEST of `repeats`: the
        // fastest run is the one least contaminated by whatever else the
        // machine was doing, and this harness is meant to be usable on a
        // machine that is not idle.
        let mut matches = 0usize;
        let _ = store.query(&filter).expect("content query");
        let mut best = Duration::MAX;
        for _ in 0..repeats.max(1) {
            let started = Instant::now();
            let found = store.query(&filter).expect("content query");
            best = best.min(started.elapsed());
            matches = found.len();
        }
        latencies.push((label, best, matches));
    }

    Cell {
        ingest,
        disk_bytes: stats.disk_bytes,
        segments: stats.segment_count,
        resident_index_bytes: store.resident_index_bytes().expect("index bytes"),
        content_resident_bytes: store.resident_content_index_bytes().expect("content bytes"),
        summary_fill: store.content_summary_fill().expect("fill"),
        text_bytes,
        latencies,
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Traza content-search benchmark\n\n\
             Usage: content-bench [--spans N] [--words N] [--repeats N]\n\n\
             Runs the same corpus with the content index on and off and reports\n\
             the difference. Defaults: 100000 spans, 60 words per field, 3 repeats."
        );
        return;
    }
    let mut spans = 100_000usize;
    let mut words = 60usize;
    let mut repeats = 3usize;
    let mut i = 0;
    while i < args.len() {
        let next = |i: usize| -> usize {
            args.get(i + 1)
                .unwrap_or_else(|| panic!("{} needs a value", args[i]))
                .parse()
                .expect("integer")
        };
        match args[i].as_str() {
            "--spans" => {
                spans = next(i);
                i += 1;
            }
            "--words" => {
                words = next(i);
                i += 1;
            }
            "--repeats" => {
                repeats = next(i);
                i += 1;
            }
            other => panic!("unknown argument {other}"),
        }
        i += 1;
    }

    let directory =
        std::env::temp_dir().join(format!("traza-content-bench-{}", std::process::id()));
    println!(
        "machine: {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("load average at start: {}", load_average());
    println!("corpus: {spans} spans, {words} words per prompt and completion");
    println!("running with the content index ...");
    let indexed = run(&directory, spans, words, true, repeats);
    println!("running without it ...");
    let plain = run(&directory, spans, words, false, repeats);

    println!(
        "\ntext ingested: {:.1} MiB across {} segments\n",
        mib(indexed.text_bytes),
        indexed.segments
    );

    println!("| query | matches | with index | without index | speedup |");
    println!("|---|---:|---:|---:|---:|");
    for step in 0..QUERIES.len() {
        let (label, with, matches) = indexed.latencies[step];
        let without = plain.latencies[step].1;
        assert_eq!(
            matches, plain.latencies[step].2,
            "the index must not change the answer for {label}"
        );
        println!(
            "| {label} | {matches} | {:.3} ms | {:.3} ms | {:.1}x |",
            ms(with),
            ms(without),
            ms(without) / ms(with).max(f64::MIN_POSITIVE)
        );
    }

    println!("\n| cost | with index | without index |");
    println!("|---|---:|---:|");
    println!(
        "| ingest + seal | {:.2} s | {:.2} s |",
        indexed.ingest.as_secs_f64(),
        plain.ingest.as_secs_f64()
    );
    println!(
        "| segment bytes | {:.1} MiB | {:.1} MiB |",
        mib(indexed.disk_bytes),
        mib(plain.disk_bytes)
    );
    println!(
        "| resident index | {:.2} MiB | {:.2} MiB |",
        mib(indexed.resident_index_bytes as u64),
        mib(plain.resident_index_bytes as u64)
    );
    println!(
        "| of which content filters | {:.2} MiB | {:.2} MiB |",
        mib(indexed.content_resident_bytes as u64),
        mib(plain.content_resident_bytes as u64)
    );
    match indexed.summary_fill {
        Some(fill) => println!(
            "\nsummary filter fill: {:.1}% (a filter approaching 100% has \
             saturated and stopped pruning)",
            fill * 100.0
        ),
        None => println!("\nno content index was built"),
    }
    println!(
        "index overhead on disk: {:.1}%",
        (indexed.disk_bytes as f64 / plain.disk_bytes.max(1) as f64 - 1.0) * 100.0
    );
    println!("load average at end: {}", load_average());
    let _ = std::fs::remove_dir_all(&directory);
}

/// The 1/5/15-minute load average, so a figure taken on a busy machine says
/// so on its own record rather than in someone's memory.
fn load_average() -> String {
    let output = std::process::Command::new("uptime").output();
    let Ok(output) = output else {
        return "unknown".to_owned();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.split("load averages:")
        .nth(1)
        .or_else(|| text.split("load average:").nth(1))
        .map(|rest| rest.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
