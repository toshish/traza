//! Search semantics: the predicates, and the planning decisions behind them.
//!
//! Four things are asserted here, each of which was previously either wrong or
//! absent:
//!
//! - **Type-agnostic attribute equality.** `attr.code=200` used to match the
//!   NUMBER 200 and never the STRING "200", so a store of stringified codes
//!   answered every such query with nothing — and an empty result set is
//!   indistinguishable from no such data.
//! - **Timestamp pruning.** `since`/`until` were pure post-filters, so a
//!   "last N minutes" query opened every segment in the store.
//! - **Probe selection by selectivity.** Only one predicate can drive the
//!   scan; the planner used to take a fixed order that tends to pick the
//!   least selective one.
//! - **Range, negation and ordering**, none of which existed, so cost and
//!   token analytics could aggregate what search could not find.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::{Config, Span, SpanFilter, SpanSort, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-search-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn store(label: &str) -> (Store, PathBuf) {
    let dir = test_dir(label);
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("opens");
    (store, dir)
}

fn span(id: &str, start_ns: u64, duration_ns: u64, attributes: Value) -> Span {
    serde_json::from_value(json!({
        "trace_id": format!("t-{id}"),
        "span_id": id,
        "name": "op",
        "service": "svc",
        "start_time_ns": start_ns,
        "end_time_ns": start_ns + duration_ns,
        "attributes": attributes,
    }))
    .expect("span")
}

fn ids(spans: &[Span]) -> Vec<&str> {
    spans.iter().map(|span| span.span_id.as_str()).collect()
}

#[test]
fn an_attribute_filter_matches_regardless_of_how_the_value_was_typed() {
    // The bug this replaces: instrumentation is inconsistent about whether a
    // status code or a token count is a number or a string, and a filter that
    // understood only one encoding returned nothing for the other while
    // looking exactly like "no such data".
    let (store, dir) = store("typing");
    store
        .ingest(span("numeric", 1_000, 10, json!({"code": 200})))
        .expect("ingest");
    store
        .ingest(span("stringy", 2_000, 10, json!({"code": "200"})))
        .expect("ingest");
    store
        .ingest(span("other", 3_000, 10, json!({"code": 404})))
        .expect("ingest");

    for probe in [json!(200), json!("200")] {
        let found = store
            .query(&SpanFilter {
                attributes: vec![("code".to_owned(), probe.clone())],
                limit: None,
                ..SpanFilter::default()
            })
            .expect("query");
        let mut found = ids(&found);
        found.sort_unstable();
        assert_eq!(
            found,
            ["numeric", "stringy"],
            "probing with {probe} must find both encodings"
        );
    }

    // Still exact: 200 does not match 404, and a prefix is not a match.
    let found = store
        .query(&SpanFilter {
            attributes: vec![("code".to_owned(), json!("20"))],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert!(
        found.is_empty(),
        "equality is not prefix matching: {found:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_time_range_skips_segments_that_cannot_match() {
    // Each flush seals one segment with a known timestamp range. A query
    // outside every range must return nothing, and one inside exactly one
    // range must return only that segment's spans — which is observable
    // proof the range is being consulted rather than every record scanned.
    let (store, dir) = store("time-prune");
    for (index, base) in [1_000_u64, 10_000, 20_000].iter().enumerate() {
        for offset in 0..5 {
            store
                .ingest(span(
                    &format!("s{index}-{offset}"),
                    base + offset,
                    10,
                    json!({"batch": index}),
                ))
                .expect("ingest");
        }
        store.flush().expect("flush");
    }
    assert_eq!(store.stats().expect("stats").segment_count, 3);

    let window = |since: u64, until: u64| {
        store
            .query(&SpanFilter {
                since_ns: Some(since),
                until_ns: Some(until),
                limit: None,
                ..SpanFilter::default()
            })
            .expect("query")
    };

    let middle = window(10_000, 10_004);
    assert_eq!(middle.len(), 5, "only the middle segment overlaps");
    assert!(middle.iter().all(|span| span.span_id.starts_with("s1-")));

    assert!(
        window(500, 900).is_empty(),
        "a window before every segment matches nothing"
    );
    assert!(
        window(50_000, 60_000).is_empty(),
        "a window after every segment matches nothing"
    );
    assert_eq!(
        window(0, u64::MAX).len(),
        15,
        "an unbounded window still returns everything"
    );

    // Results alone cannot prove pruning happened — a scanned segment and a
    // skipped one produce the same answer — so assert the counter. Without
    // this the test passes with pruning entirely disabled, which is exactly
    // what a mutation check showed.
    let pruned_before = store.metrics().segments_pruned_by_time.get();
    let _ = window(10_000, 10_004);
    let pruned = store.metrics().segments_pruned_by_time.get() - pruned_before;
    assert_eq!(
        pruned, 2,
        "the two non-overlapping segments must be skipped, not scanned"
    );

    let pruned_before = store.metrics().segments_pruned_by_time.get();
    let _ = window(0, u64::MAX);
    assert_eq!(
        store.metrics().segments_pruned_by_time.get() - pruned_before,
        0,
        "an unbounded window can rule nothing out"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn adding_a_selective_filter_never_widens_the_scan() {
    // Only one predicate drives the index probe. The planner used to take a
    // fixed order — service, then name, then the first attribute — so adding
    // a precise attribute to a service query left the scan driven by the
    // least selective term. Correctness is what is asserted here; the
    // selectivity choice is a performance property proven by the unit test on
    // posting lengths.
    let (store, dir) = store("selectivity");
    for index in 0..50 {
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index,
                10,
                json!({"rare": if index == 7 { "yes" } else { "no" }}),
            ))
            .expect("ingest");
    }
    store.flush().expect("flush");

    let broad = store
        .query(&SpanFilter {
            service: Some("svc".to_owned()),
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(broad.len(), 50, "service alone matches everything");

    let narrowed = store
        .query(&SpanFilter {
            service: Some("svc".to_owned()),
            attributes: vec![("rare".to_owned(), json!("yes"))],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(ids(&narrowed), ["s7"], "the selective term still applies");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn range_and_negation_predicates_find_what_analytics_can_already_count() {
    let (store, dir) = store("ranges");
    store
        .ingest(span(
            "cheap",
            1_000,
            10,
            json!({"llm.cost_usd": 0.001, "status": "ok"}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "pricey",
            2_000,
            10,
            json!({"llm.cost_usd": 0.25, "status": "error"}),
        ))
        .expect("ingest");
    // Stringified, as several SDKs emit it — a range filter that understood
    // only JSON numbers would silently skip this one.
    store
        .ingest(span(
            "pricey-string",
            3_000,
            10,
            json!({"llm.cost_usd": "0.30"}),
        ))
        .expect("ingest");

    let expensive = store
        .query(&SpanFilter {
            min_attributes: vec![("llm.cost_usd".to_owned(), 0.05)],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    let mut found = ids(&expensive);
    found.sort_unstable();
    assert_eq!(
        found,
        ["pricey", "pricey-string"],
        "a numeric bound reads stringified numbers too"
    );

    let bounded = store
        .query(&SpanFilter {
            max_attributes: vec![("llm.cost_usd".to_owned(), 0.01)],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(ids(&bounded), ["cheap"]);

    // A span with no `status` at all is NOT an error, so it must survive the
    // exclusion. Treating a missing key as a match would hide most of a
    // corpus behind a filter that reads like it only removes failures.
    let not_error = store
        .query(&SpanFilter {
            excluded_attributes: vec![("status".to_owned(), json!("error"))],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    let mut found = ids(&not_error);
    found.sort_unstable();
    assert_eq!(found, ["cheap", "pricey-string"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sorting_ranks_every_match_not_just_the_first_page() {
    // The trap a sorted query has to avoid: taking `limit` matches in scan
    // order and sorting those. The slowest span here is ingested LAST, so a
    // sort applied after truncation would never see it.
    let (store, dir) = store("sort");
    for index in 0..20_u64 {
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index,
                (index + 1) * 100,
                json!({}),
            ))
            .expect("ingest");
    }

    let slowest = store
        .query(&SpanFilter {
            sort: Some(SpanSort::DurationDesc),
            limit: Some(3),
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(
        ids(&slowest),
        ["s19", "s18", "s17"],
        "the slowest three, found despite being last in scan order"
    );

    let fastest = store
        .query(&SpanFilter {
            sort: Some(SpanSort::DurationAsc),
            limit: Some(2),
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(ids(&fastest), ["s0", "s1"]);

    let newest = store
        .query(&SpanFilter {
            sort: Some(SpanSort::StartDesc),
            limit: Some(1),
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(ids(&newest), ["s19"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unsorted_query_keeps_its_stable_order() {
    // Sorting is opt-in precisely because the default can stream. Adding the
    // capability must not quietly reorder every existing caller's results.
    let (store, dir) = store("stable");
    for index in [3_u64, 1, 2] {
        store
            .ingest(span(&format!("s{index}"), 1_000 + index, 10, json!({})))
            .expect("ingest");
    }
    let spans = store
        .query(&SpanFilter {
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(
        ids(&spans),
        ["s1", "s2", "s3"],
        "default order is by start time, unchanged"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_sort_over_too_many_matches_is_refused_rather_than_answered_wrongly() {
    // Ranking requires seeing every match. Past the ceiling the honest answer
    // is an error: a "slowest ten" computed over an arbitrary subset is a
    // wrong answer that looks like a right one.
    let (store, dir) = store("sort-ceiling");
    let limit = traza::SORT_CANDIDATE_LIMIT;
    assert!(limit > 0);
    // Build just past the ceiling.
    let mut batch = Vec::with_capacity(limit + 1);
    for index in 0..=limit {
        batch.push(span(
            &format!("s{index}"),
            1_000 + index as u64,
            10,
            json!({}),
        ));
    }
    store.ingest_batch(batch).expect("ingest");

    let result = store.query(&SpanFilter {
        sort: Some(SpanSort::DurationDesc),
        limit: Some(10),
        ..SpanFilter::default()
    });
    match result {
        Err(traza::Error::QueryTooBroad(message)) => {
            assert!(
                message.contains("narrow the filter"),
                "the refusal should say what to do: {message}"
            );
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(spans) => panic!("expected refusal, got {} spans", spans.len()),
    }

    // The same query is fine once it is narrowed.
    let narrowed = store
        .query(&SpanFilter {
            sort: Some(SpanSort::DurationDesc),
            since_ns: Some(1_000),
            until_ns: Some(1_010),
            limit: Some(3),
            ..SpanFilter::default()
        })
        .expect("a narrowed sort is answered");
    assert_eq!(narrowed.len(), 3);
    let _ = std::fs::remove_dir_all(dir);
}
