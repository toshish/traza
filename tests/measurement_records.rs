//! Published figures must match the committed measurement records.
//!
//! The failure this prevents: the index-memory record was regenerated from a
//! corrected harness, and the prose derived from the PREVIOUS run stayed
//! exactly as it was. The document then cited a record that contradicted it,
//! which is worse than citing nothing — the citation is what makes the stale
//! number look verified.
//!
//! These tests read the committed JSON and check the prose against it, so
//! regenerating a record without updating what quotes it fails the build.

use std::collections::HashSet;

use serde_json::Value;

fn record() -> Value {
    let raw = std::fs::read_to_string("INDEX-MEM-BENCHMARK.json")
        .expect("INDEX-MEM-BENCHMARK.json is a committed measurement record");
    serde_json::from_str(&raw).expect("the record parses")
}

fn capacity_guide() -> String {
    std::fs::read_to_string("docs/operations/capacity.md").expect("capacity guide")
}

/// MiB, rounded and comma-grouped the way the prose writes it.
fn mib(bytes: f64) -> String {
    let value = (bytes / (1024.0 * 1024.0)).round() as u64;
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[test]
fn every_compaction_peak_the_guide_quotes_exists_in_the_record() {
    let record = record();
    let guide = capacity_guide();

    // Every RSS figure a completed merge produced: the guide's table quotes
    // open, peak and settled, and all three must come from the record.
    let mut known: HashSet<String> = HashSet::new();
    for result in record["results"].as_array().expect("results") {
        let Some(compacted) = result.get("compacted") else {
            continue;
        };
        if compacted["compacted_away"].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        for field in [
            "rss_open",
            "rss_peak_during_compaction",
            "rss_settled_after_compaction",
        ] {
            let value = compacted[field]
                .as_f64()
                .unwrap_or_else(|| panic!("a merge that ran records {field}"));
            known.insert(mib(value));
        }
    }
    assert!(!known.is_empty(), "the record contains no completed merges");

    // Any four-digit MiB figure in the compaction section must be one of them.
    // A stale number from an earlier run fails here, which is the whole point.
    let section = guide
        .split("RSS rises sharply with")
        .nth(1)
        .expect("the guide discusses the compaction transient");
    let section = section.split("## ").next().unwrap_or(section);
    // Split on anything that is not a digit or a comma, so a range written
    // "1,204-1,601 MiB" is checked as two figures rather than one nonsense
    // token.
    for candidate in section.split(|c: char| !c.is_ascii_digit() && c != ',') {
        let candidate = candidate.trim_matches(',');
        if candidate.len() >= 5 && candidate.contains(',') {
            assert!(
                known.contains(candidate),
                "the capacity guide quotes {candidate} MiB in its compaction section, but no \
                 completed merge in the committed record produced that figure. Known values: \
                 {known:?}. Regenerating the record without updating the prose is how these \
                 drift apart."
            );
        }
    }
}

#[test]
fn the_record_describes_the_run_that_produced_it() {
    let record = record();
    assert_eq!(
        record["result_count"].as_u64().expect("result_count"),
        record["results"].as_array().expect("results").len() as u64,
        "the record's declared cell count must match the cells it carries"
    );
    assert_ne!(
        record["commit"].as_str().expect("commit"),
        "unknown",
        "a record with no provenance is not a record"
    );
    assert!(
        !record["dirty_tree"].as_bool().expect("dirty_tree"),
        "a committed record must come from a clean tree"
    );
    let command = record["command"].as_str().expect("command");
    assert!(
        command.contains("--matrix"),
        "a canonical record is a matrix run, got: {command}"
    );
}
