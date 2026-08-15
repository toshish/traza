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

// ---------------------------------------------------------------- content

/// A store whose segments are small enough that several exist, so that segment
/// and block pruning have something to prune.
fn content_store(label: &str) -> (Store, PathBuf) {
    let dir = test_dir(label);
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            flush_spans: 200,
            ..Config::default()
        },
    )
    .expect("opens");
    (store, dir)
}

#[test]
fn content_search_finds_words_anywhere_in_a_spans_text() {
    let (store, dir) = content_store("content-basic");
    store
        .ingest(span(
            "prompt",
            1_000,
            10,
            json!({"gen_ai.prompt": "Please issue a refund for order 4471"}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "completion",
            2_000,
            10,
            json!({"gen_ai.completion": "I have processed the REFUND."}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "nested",
            3_000,
            10,
            json!({"gen_ai.input.messages": [
                {"role": "user", "content": "where is my refund"},
                {"role": "assistant", "content": "checking"}
            ]}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "unrelated",
            4_000,
            10,
            json!({"gen_ai.prompt": "summarize the quarterly report"}),
        ))
        .expect("ingest");
    store.flush().expect("flush");

    let found = store
        .query(&SpanFilter {
            content: Some("refund".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    let mut got = ids(&found);
    got.sort_unstable();
    assert_eq!(
        got,
        ["completion", "nested", "prompt"],
        "case, punctuation and nesting must not hide a match"
    );

    // A conjunction, not a phrase: both words must be present, anywhere.
    let both = store
        .query(&SpanFilter {
            content: Some("refund order".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&both), ["prompt"]);

    let none = store
        .query(&SpanFilter {
            content: Some("refund chargeback".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert!(none.is_empty(), "a word no span holds excludes every span");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn content_search_is_word_matching_not_substring_matching() {
    // This is the semantic the Bloom filter can safely over-approximate. If
    // "refund" matched "refunds", the index would skip that span and the
    // answer would be WRONG rather than slow -- see src/content.rs.
    let (store, dir) = content_store("content-words");
    store
        .ingest(span(
            "plural",
            1_000,
            10,
            json!({"note": "refunds were issued"}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "prefixed",
            2_000,
            10,
            json!({"note": "prerefund hold"}),
        ))
        .expect("ingest");
    store
        .ingest(span("exact", 3_000, 10, json!({"note": "refund issued"})))
        .expect("ingest");
    store.flush().expect("flush");

    let found = store
        .query(&SpanFilter {
            content: Some("refund".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(
        ids(&found),
        ["exact"],
        "only the whole word matches; the index cannot support substrings"
    );

    // And the words that ARE whole still resolve.
    let plural = store
        .query(&SpanFilter {
            content: Some("refunds".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&plural), ["plural"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn content_search_reaches_buffered_spans_and_events() {
    let (store, dir) = content_store("content-buffer");
    let mut with_event: Span = span("evented", 1_000, 10, json!({}));
    with_event.events.push(
        serde_json::from_value(json!({
            "name": "tool.call",
            "timestamp_ns": 1_005,
            "attributes": {"arguments": "{\"city\":\"Lisbon\"}"}
        }))
        .expect("event"),
    );
    store.ingest(with_event).expect("ingest");
    store
        .ingest(span(
            "buffered",
            2_000,
            10,
            json!({"note": "unflushed lisbon"}),
        ))
        .expect("ingest");

    // Nothing is flushed: both spans are in the write buffer, which has no
    // index at all and must still answer.
    let found = store
        .query(&SpanFilter {
            content: Some("lisbon".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    let mut got = ids(&found);
    got.sort_unstable();
    assert_eq!(got, ["buffered", "evented"]);

    // An event NAME is searchable too.
    let by_event = store
        .query(&SpanFilter {
            content: Some("tool call".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&by_event), ["evented"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_content_query_the_index_cannot_help_with_still_answers() {
    // Punctuation and non-ASCII tokenize to nothing. The planner must scan
    // rather than let an empty conjunction admit or reject everything.
    let (store, dir) = content_store("content-unindexable");
    store
        .ingest(span("cjk", 1_000, 10, json!({"note": "世界 hello"})))
        .expect("ingest");
    store.flush().expect("flush");

    let unindexable = store
        .query(&SpanFilter {
            content: Some("世界".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert!(
        unindexable.is_empty(),
        "no tokens means no match, not every match"
    );

    // The ASCII word beside it is found normally.
    let ascii = store
        .query(&SpanFilter {
            content: Some("hello".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&ascii), ["cjk"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn content_search_composes_with_the_other_predicates() {
    let (store, dir) = content_store("content-compose");
    for index in 0..10u64 {
        let mut s = span(
            &format!("s{index}"),
            1_000 + index * 1_000,
            10 + index,
            json!({"note": "refund requested", "tier": if index % 2 == 0 { "gold" } else { "silver" }}),
        );
        s.service = format!("svc-{}", index % 3);
        store.ingest(s).expect("ingest");
    }
    store.flush().expect("flush");

    let narrowed = store
        .query(&SpanFilter {
            content: Some("refund".to_owned()),
            attributes: vec![("tier".to_owned(), json!("gold"))],
            since_ns: Some(3_000),
            ..SpanFilter::default()
        })
        .expect("content query");
    let mut got = ids(&narrowed);
    got.sort_unstable();
    assert_eq!(got, ["s2", "s4", "s6", "s8"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_content_index_prunes_segments_and_blocks_rather_than_scanning() {
    // Correctness cannot detect this. A store that ignored the content index
    // entirely and checked every span by hand returns byte-identical results;
    // the only difference is how much it read. So the assertions here are on
    // the counters, and a mutation check confirmed that disabling either the
    // segment-level or the block-level filter fails them.
    // Segments must be several BLOCKS wide for block pruning to have anything
    // to prove: at 200 spans a segment is two blocks, and admitting both
    // looks the same as admitting one. 2,000 spans is sixteen blocks.
    let dir = test_dir("content-pruning");
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            flush_spans: 2_000,
            ..Config::default()
        },
    )
    .expect("opens");
    let total = 8_000usize;
    for index in 0..total {
        let note = if index == 2_500 {
            "the antidisestablishment clause applies".to_owned()
        } else {
            format!("routine completion number {index} about weather and traffic")
        };
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index as u64,
                10,
                json!({ "note": note }),
            ))
            .expect("ingest");
    }
    store.flush().expect("flush");
    let segments = store.stats().expect("stats").segment_count;
    assert!(segments >= 4, "expected several segments, got {segments}");

    let pruned_before = store.metrics().segments_pruned_by_content.get();
    let admitted_before = store.metrics().records_admitted_by_content.get();
    let found = store
        .query(&SpanFilter {
            content: Some("antidisestablishment".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&found), ["s2500"], "the answer is still exactly right");

    let pruned = store.metrics().segments_pruned_by_content.get() - pruned_before;
    let admitted = store.metrics().records_admitted_by_content.get() - admitted_before;

    // The resident summary filter should eliminate nearly every segment
    // without touching the file.
    assert!(
        pruned as usize >= segments - 2,
        "the summary filter must skip almost every segment: pruned {pruned} of {segments}"
    );
    // And within whatever survives, the bit-sliced block filters must narrow
    // to a block or two rather than admitting the segment whole. A segment is
    // 2,000 records here, so anything near that means the block filters are
    // not being consulted.
    assert!(
        admitted <= 384,
        "block filters must narrow to a few blocks, not a whole 2,000-record \
         segment: {admitted} records admitted out of {total}"
    );
    assert!(
        admitted >= 1,
        "the block holding the match must be admitted"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_saturated_or_absent_content_index_is_slow_and_never_wrong() {
    // The failure mode that must stay safe: a segment with no content index
    // at all -- written before v5, or holding no indexable text -- must be
    // scanned, not skipped. Skipping it would turn content search into a
    // query that silently returns nothing.
    let (store, dir) = content_store("content-absent");
    // Spans whose only text is non-ASCII produce no tokens, so their segments
    // carry no usable content index.
    for index in 0..300usize {
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index as u64,
                10,
                json!({ "note": format!("世界{index}") }),
            ))
            .expect("ingest");
    }
    // One span in the same store does carry an indexable word.
    store
        .ingest(span("target", 9_000, 10, json!({"note": "findable"})))
        .expect("ingest");
    store.flush().expect("flush");

    let found = store
        .query(&SpanFilter {
            content: Some("findable".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&found), ["target"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compaction_preserves_the_content_index() {
    // A merge that silently dropped the content index would still return
    // every correct row -- it would just scan for them. So this asserts the
    // counter, not the result: without it, the failure mode is a store that
    // gets slower the longer it runs and never says why.
    let dir = test_dir("content-compaction");
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: Some(traza::CompactionConfig {
                fanout: 2,
                base_bytes: 1_024 * 1_024,
                max_segment_bytes: 256 * 1_024 * 1_024,
            }),
            flush_spans: 2_000,
            ..Config::default()
        },
    )
    .expect("opens");

    for index in 0..8_000usize {
        let note = if index == 5_000 {
            "the antidisestablishment clause applies".to_owned()
        } else {
            format!("routine completion number {index} about weather")
        };
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index as u64,
                10,
                json!({ "note": note }),
            ))
            .expect("ingest");
    }
    store.flush().expect("flush");
    let merged = store.compact_segments().expect("compact");
    assert!(
        merged > 0,
        "the corpus must actually merge for this to test anything"
    );

    let pruned_before = store.metrics().segments_pruned_by_content.get();
    let admitted_before = store.metrics().records_admitted_by_content.get();
    let found = store
        .query(&SpanFilter {
            content: Some("antidisestablishment".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&found), ["s5000"], "the answer survives the merge");

    let admitted = store.metrics().records_admitted_by_content.get() - admitted_before;
    let pruned = store.metrics().segments_pruned_by_content.get() - pruned_before;

    // The lower bound is the load-bearing half. `records_admitted_by_content`
    // only counts when a content index was actually consulted, so "admitted
    // is small" is satisfied just as well by a merge that dropped the index
    // entirely and admitted nothing through it -- which is exactly what a
    // mutation check showed the first version of this test accepting.
    assert!(
        admitted >= 1,
        "a merged segment must still carry a content index: nothing was \
         admitted through one ({pruned} segments pruned)"
    );
    assert!(
        admitted <= 384,
        "the merged segment's block filters must still narrow: {admitted} records"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn content_pruning_does_not_make_a_segment_supersede_itself() {
    // Regression, P1. Limited queries map source index -> segment index
    // POSITIONALLY. Every `continue` in the segment loop compresses `sources`
    // without compressing `segments`, so a surviving segment is checked for
    // supersedence against ITSELF and every row it holds is dropped.
    //
    // Content pruning made this reachable on the default path -- an HTTP
    // query carries limit=100 -- but time pruning could already trigger it,
    // so both are asserted here.
    let (store, dir) = content_store("prune-supersede");
    // Segment 0: no needle. Segment 1: the needle.
    for index in 0..200usize {
        store
            .ingest(span(
                &format!("old{index}"),
                1_000 + index as u64,
                10,
                json!({"note": "ordinary completion text"}),
            ))
            .expect("ingest");
    }
    store.flush().expect("flush");
    store
        .ingest(span(
            "target",
            900_000,
            10,
            json!({"note": "rareneedle here"}),
        ))
        .expect("ingest");
    store.flush().expect("flush");
    assert_eq!(store.stats().expect("stats").segment_count, 2);

    let found = store
        .query(&SpanFilter {
            content: Some("rareneedle".to_owned()),
            limit: Some(100),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(
        ids(&found),
        ["target"],
        "the surviving segment's row must not be dropped as superseded by itself"
    );

    // The same shape via time pruning, with no content filter involved.
    let by_time = store
        .query(&SpanFilter {
            since_ns: Some(800_000),
            limit: Some(100),
            ..SpanFilter::default()
        })
        .expect("time query");
    assert_eq!(
        ids(&by_time),
        ["target"],
        "time pruning must not shift the supersedence mapping either"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn attribute_typing_is_agnostic_after_sealing_too() {
    // Regression. `attr.code=200` is documented to match the number 200 and
    // the string "200". It did -- in the write buffer. The segment index is
    // keyed on the CANONICAL JSON of the value, so probing with `200` finds
    // only records that stored a number, and the string record is never a
    // candidate. The index was acting as a filter rather than a superset.
    //
    // The existing test for this never flushed, so it only ever exercised
    // the buffer.
    let (store, dir) = store("typing-sealed");
    store
        .ingest(span("numeric", 1_000, 10, json!({"code": 200})))
        .expect("ingest");
    store
        .ingest(span("stringy", 2_000, 10, json!({"code": "200"})))
        .expect("ingest");
    store.flush().expect("flush");
    assert!(store.stats().expect("stats").segment_count > 0);

    for probe in [json!(200), json!("200")] {
        let found = store
            .query(&SpanFilter {
                attributes: vec![("code".to_owned(), probe.clone())],
                limit: None,
                ..SpanFilter::default()
            })
            .expect("query");
        let mut got = ids(&found);
        got.sort_unstable();
        assert_eq!(
            got,
            ["numeric", "stringy"],
            "probing a sealed segment with {probe} must find both encodings"
        );
    }

    // Still exact after sealing: a prefix is not a match.
    assert!(store
        .query(&SpanFilter {
            attributes: vec![("code".to_owned(), json!("20"))],
            limit: None,
            ..SpanFilter::default()
        })
        .expect("query")
        .is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_offloaded_value_is_searchable_only_within_its_preview() {
    // Offloading runs at ingest, before anything indexes the span, so the
    // text beyond the inline preview is simply not present for either the
    // index or the match to see. That is a bounded feature, not a wrong one:
    // both sides read the span through the same function, so a span is never
    // SKIPPED that would have matched. This pins the boundary so the
    // documented limit stays the real one.
    let dir = test_dir("content-offload");
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            payload_threshold: Some(512),
            ..Config::default()
        },
    )
    .expect("opens");

    // "earlyword" lands inside the 256-character preview; "lateword" does not.
    let mut text = String::from("earlyword ");
    text.push_str(&"filler ".repeat(200));
    text.push_str("lateword");
    assert!(text.len() > 512, "the value must actually offload");
    store
        .ingest(span("big", 1_000, 10, json!({ "gen_ai.prompt": text })))
        .expect("ingest");
    store.flush().expect("flush");

    let early = store
        .query(&SpanFilter {
            content: Some("earlyword".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(ids(&early), ["big"], "the preview is searchable");

    let late = store
        .query(&SpanFilter {
            content: Some("lateword".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert!(
        late.is_empty(),
        "text past the preview is not indexed, and this is the documented limit"
    );

    // The `$payload` hash is not indexed as if it were prose.
    let hashish = store
        .query(&SpanFilter {
            content: Some("sha256".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert!(hashish.is_empty(), "the reference itself is not content");

    // Below the threshold nothing offloads and the whole value is searchable,
    // which is the default posture at 256 KiB.
    let inline_dir = test_dir("content-inline");
    let inline = Store::open(
        &inline_dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            payload_threshold: Some(1024 * 1024),
            ..Config::default()
        },
    )
    .expect("opens");
    let mut wide = String::from("earlyword ");
    wide.push_str(&"filler ".repeat(200));
    wide.push_str("lateword");
    inline
        .ingest(span("small", 1_000, 10, json!({ "gen_ai.prompt": wide })))
        .expect("ingest");
    inline.flush().expect("flush");
    assert_eq!(
        ids(&inline
            .query(&SpanFilter {
                content: Some("lateword".to_owned()),
                ..SpanFilter::default()
            })
            .expect("content query")),
        ["small"],
        "an inline value is searchable in full"
    );
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(inline_dir);
}

#[test]
fn the_content_admission_metric_counts_only_records_the_query_reads() {
    // The counter is meant to answer "how much did the content index make us
    // decode". It used to be incremented as soon as the content candidates
    // were COMPUTED, before the planner compared them against the attribute
    // probes -- so a query narrowed by an attribute still reported the whole
    // content candidate list as admitted, and the selectivity signal read as
    // though the index were far worse than it is.
    let (store, dir) = content_store("content-metric");
    for index in 0..2_000usize {
        store
            .ingest(span(
                &format!("s{index}"),
                1_000 + index as u64,
                10,
                json!({
                    // Every span carries the word, so content alone is useless.
                    "note": "shared completion text",
                    // Exactly one carries the attribute.
                    "rare": if index == 7 { "yes" } else { "no" },
                }),
            ))
            .expect("ingest");
    }
    store.flush().expect("flush");

    let before = store.metrics().records_admitted_by_content.get();
    let found = store
        .query(&SpanFilter {
            content: Some("shared".to_owned()),
            attributes: vec![("rare".to_owned(), json!("yes"))],
            ..SpanFilter::default()
        })
        .expect("query");
    assert_eq!(ids(&found), ["s7"], "the answer is unaffected");
    let admitted = store.metrics().records_admitted_by_content.get() - before;
    assert_eq!(
        admitted, 0,
        "the attribute probe won, so no record was read on the content \
         index's account: {admitted} counted"
    );

    // When content DOES drive the scan, it is counted.
    let before = store.metrics().records_admitted_by_content.get();
    let _ = store
        .query(&SpanFilter {
            content: Some("shared".to_owned()),
            ..SpanFilter::default()
        })
        .expect("query");
    assert!(
        store.metrics().records_admitted_by_content.get() > before,
        "a content-driven scan must still be counted"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_user_object_carrying_a_payload_key_is_still_searchable() {
    // `$payload` is Traza's marker for an offloaded value, but nothing stops
    // a tool call's arguments from having a field of that name. Treating any
    // object that has one as a reference -- and indexing only its `preview` --
    // silently removed every other field of that object from search.
    let (store, dir) = content_store("content-payload-key");
    store
        .ingest(span(
            "tool",
            1_000,
            10,
            json!({"arguments": {"$payload": "business-value", "query": "nestedneedle"}}),
        ))
        .expect("ingest");
    store.flush().expect("flush");

    let found = store
        .query(&SpanFilter {
            content: Some("nestedneedle".to_owned()),
            ..SpanFilter::default()
        })
        .expect("content query");
    assert_eq!(
        ids(&found),
        ["tool"],
        "a sibling field of a $payload key must still be indexed"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_genuinely_offloaded_value_still_indexes_only_its_preview() {
    // The other side of the same change: relaxing the reference check must
    // not start indexing the whole reference object.
    let dir = test_dir("content-offload-preview");
    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            payload_threshold: Some(512),
            ..Config::default()
        },
    )
    .expect("opens");
    let mut text = String::from("earlyword ");
    text.push_str(&"filler ".repeat(200));
    text.push_str("lateword");
    store
        .ingest(span("big", 1_000, 10, json!({ "gen_ai.prompt": text })))
        .expect("ingest");
    store.flush().expect("flush");

    let probe = |needle: &str| {
        store
            .query(&SpanFilter {
                content: Some(needle.to_owned()),
                ..SpanFilter::default()
            })
            .expect("content query")
    };
    assert_eq!(ids(&probe("earlyword")), ["big"], "the preview is indexed");
    assert!(probe("lateword").is_empty(), "text past the preview is not");
    assert!(
        probe("sha256").is_empty(),
        "the reference hash is not indexed as prose"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A version of one primary key under the one trace every version shares —
/// the upsert corpus below is a single long trace being rewritten, which is
/// what makes an unprefiltered supersede probe expensive: probing a key
/// decodes its trace's whole slice of the probed segment.
fn upsert_span(span_id: &str, name: &str, start_ns: u64) -> Span {
    serde_json::from_value(json!({
        "trace_id": "upsert-trace",
        "span_id": span_id,
        "name": name,
        "service": "svc",
        "start_time_ns": start_ns,
        "end_time_ns": start_ns + 10,
        "attributes": {},
    }))
    .expect("span")
}

#[test]
fn a_duplicate_heavy_store_probes_only_keys_a_newer_segment_actually_holds() {
    // The shape a crash leaves behind: until compaction runs, a recovered
    // store carries every superseded version of its hot keys. Resolving
    // last-write-wins used to probe every newer segment for every candidate,
    // so exactly the store that most needs its first queries to be cheap paid
    // matches × segments × trace-width decodes for them. The key-hash
    // prefilter bounds that: a candidate pays an exact probe only where a
    // newer segment provably holds its key.
    //
    // Results alone cannot prove the bound — with or without the prefilter a
    // query returns the same rows — so each path is held to the probe
    // counter, the same way the pruning tests hold segment skipping to
    // theirs.
    const SEGMENTS: usize = 12;
    const STABLE: usize = 40;
    const HOT: usize = 6;
    // One exact probe per superseded version actually present: each old hot
    // version is confirmed dead by the first newer segment holding its key,
    // and the stable spans no later segment ever rewrote cost nothing.
    // Without the prefilter the stable spans alone pay STABLE probes per
    // pair of segments — 2,640 here, forty times this — and every one of
    // them decodes trace records to learn nothing.
    const CONFIRMING_PROBES: u64 = (HOT * (SEGMENTS - 1)) as u64;

    let (store, dir) = store("supersede-prefilter");
    for round in 0..SEGMENTS {
        let base = 1_000_000 * (round as u64 + 1);
        for item in 0..STABLE {
            let id = format!("stable-{round}-{item}");
            store
                .ingest(upsert_span(&id, "settled", base + item as u64))
                .expect("ingest");
        }
        for key in 0..HOT {
            let id = format!("hot-{key}");
            let version = format!("v{round}");
            store
                .ingest(upsert_span(&id, &version, base + (STABLE + key) as u64))
                .expect("ingest");
        }
        store.flush().expect("flush seals one segment per round");
    }

    let expect_resolved = |spans: &[Span], label: &str| {
        assert_eq!(
            spans.len(),
            SEGMENTS * STABLE + HOT,
            "{label}: every stable span, and each hot key exactly once"
        );
        let hot: Vec<&Span> = spans
            .iter()
            .filter(|span| span.span_id.starts_with("hot-"))
            .collect();
        assert_eq!(hot.len(), HOT, "{label}: one survivor per hot key");
        for span in hot {
            assert_eq!(
                span.name,
                format!("v{}", SEGMENTS - 1),
                "{label}: {} must resolve to its newest version",
                span.span_id
            );
        }
    };

    // The unlimited search path.
    let before = store.metrics().supersede_probes.get();
    let all = store.query(&SpanFilter::default()).expect("query");
    expect_resolved(&all, "unlimited");
    assert_eq!(
        store.metrics().supersede_probes.get() - before,
        CONFIRMING_PROBES,
        "an unlimited query probes once per superseded version, not per \
         candidate × segment"
    );

    // The limited path: same rows, same bound.
    let before = store.metrics().supersede_probes.get();
    let paged = store
        .query(&SpanFilter {
            limit: Some(10_000),
            ..SpanFilter::default()
        })
        .expect("query");
    expect_resolved(&paged, "limited");
    assert_eq!(
        store.metrics().supersede_probes.get() - before,
        CONFIRMING_PROBES,
        "the limited path is held to the same probe bound"
    );

    // The fold behind every aggregation route.
    let before = store.metrics().supersede_probes.get();
    let mut folded: Vec<Span> = Vec::new();
    store
        .fold_spans(&SpanFilter::default(), |span| folded.push(span.clone()))
        .expect("fold");
    expect_resolved(&folded, "fold");
    assert_eq!(
        store.metrics().supersede_probes.get() - before,
        CONFIRMING_PROBES,
        "the fold is held to the same probe bound"
    );

    // A store that lost its rollup sidecars — the other thing a crash can
    // take — answers identically: the prefilter rebuilds from the segments
    // themselves and heals the sidecars in passing for the next reader.
    drop(store);
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("rollup") {
            std::fs::remove_file(&path).expect("remove sidecar");
            removed += 1;
        }
    }
    assert_eq!(removed, SEGMENTS, "every seal left a sidecar to lose");

    let store = Store::open(
        &dir,
        Config {
            durability: traza::Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("reopens");
    let before = store.metrics().supersede_probes.get();
    let all = store.query(&SpanFilter::default()).expect("query");
    expect_resolved(&all, "after sidecar loss");
    assert_eq!(
        store.metrics().supersede_probes.get() - before,
        CONFIRMING_PROBES,
        "a sidecar-less store pays a one-time rebuild, never extra probes"
    );
    let healed = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry.path().extension().and_then(|e| e.to_str()) == Some("rollup")
            })
        })
        .count();
    // Every segment the queries consulted healed its sidecar. The OLDEST
    // segment is the one exception, and deliberately: candidates are only
    // ever probed against NEWER segments, nothing is older than the oldest,
    // so the lazy build never pays for a set no probe can ask about.
    assert_eq!(healed, SEGMENTS - 1, "the rebuild healed what it consulted");
    let _ = std::fs::remove_dir_all(dir);
}
