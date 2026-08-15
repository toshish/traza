//! Derived-cost acceptance: a store that knows the model and the token counts
//! must be able to report a cost, must never overwrite a metered one, and must
//! not keep believing counters it folded under rates that have since changed.
//!
//! The last of those is the one worth a test file. Rollups persist their
//! counters, so a derived cost is a cached value with a configuration input,
//! and a cache that cannot see its input change reports the old answer forever
//! while looking entirely healthy.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use traza::analytics::{LlmGroupBy, SessionOrder};
use traza::pricing::Pricing;
use traza::{Config, Span, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-pricing-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

const TABLE: &str = r#"{"models": {
    "gpt-5.6-sol": {"input_per_mtok": 1.0, "output_per_mtok": 10.0}
}}"#;

/// Rates doubled, so the same spans must produce exactly twice the cost.
const REPRICED: &str = r#"{"models": {
    "gpt-5.6-sol": {"input_per_mtok": 2.0, "output_per_mtok": 20.0}
}}"#;

fn config(table: Option<&str>) -> Config {
    Config {
        pricing: Arc::new(match table {
            Some(text) => Pricing::parse(text).expect("pricing parses"),
            None => Pricing::default(),
        }),
        ..Config::default()
    }
}

/// An LLM span with token counts and, optionally, a metered cost.
fn span(span_id: &str, model: &str, prompt: u64, completion: u64, metered: Option<f64>) -> Span {
    let mut attributes = json!({
        "session.id": "s1",
        "gen_ai.request.model": model,
        "gen_ai.usage.input_tokens": prompt,
        "gen_ai.usage.output_tokens": completion,
    });
    if let Some(cost) = metered {
        attributes["llm.cost_usd"] = json!(cost);
    }
    serde_json::from_value(json!({
        "trace_id": "t1", "span_id": span_id, "name": "chat", "service": "agent",
        "start_time_ns": 1_700_000_000_000_000_000u64,
        "end_time_ns": 1_700_000_000_005_000_000u64,
        "attributes": attributes,
    }))
    .expect("span")
}

/// `(cost_usd, cost_derived_usd)` for the one model row.
fn model_cost(store: &Store) -> (f64, f64) {
    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregate");
    let row = rows.first().expect("one model row");
    (row.cost_usd, row.cost_derived_usd)
}

#[test]
fn an_unpriced_store_reports_nothing_and_a_priced_one_reports_the_arithmetic() {
    // 1M in at $1/Mtok + 1M out at $10/Mtok = $11.
    let spans = vec![span("a", "gpt-5.6-sol", 1_000_000, 1_000_000, None)];

    let bare = test_dir("bare");
    let store = Store::open(&bare, config(None)).expect("opens");
    store.ingest_batch(spans.clone()).expect("append");
    assert_eq!(
        model_cost(&store),
        (0.0, 0.0),
        "with no table there is nothing to derive, which is the old behaviour exactly"
    );

    let priced = test_dir("priced");
    let store = Store::open(&priced, config(Some(TABLE))).expect("opens");
    store.ingest_batch(spans).expect("append");
    let (total, derived) = model_cost(&store);
    assert!((total - 11.0).abs() < 1e-9, "cost was {total}");
    assert!(
        (derived - 11.0).abs() < 1e-9,
        "all of it came from the table, and the response must say so: {derived}"
    );
}

#[test]
fn a_metered_cost_is_never_replaced_by_the_table() {
    let dir = test_dir("metered");
    let store = Store::open(&dir, config(Some(TABLE))).expect("opens");
    // The table would say $11; the span says it was actually charged $0.42.
    store
        .ingest_batch(vec![span(
            "a",
            "gpt-5.6-sol",
            1_000_000,
            1_000_000,
            Some(0.42),
        )])
        .expect("append");

    let (total, derived) = model_cost(&store);
    assert!((total - 0.42).abs() < 1e-9, "measurement wins: {total}");
    assert_eq!(derived, 0.0, "nothing here was derived");
}

#[test]
fn a_mixed_total_reports_how_much_of_it_is_an_estimate() {
    let dir = test_dir("mixed");
    let store = Store::open(&dir, config(Some(TABLE))).expect("opens");
    store
        .ingest_batch(vec![
            span("a", "gpt-5.6-sol", 1_000_000, 1_000_000, Some(0.42)),
            span("b", "gpt-5.6-sol", 1_000_000, 1_000_000, None),
            // No rate for this model: it contributes tokens and no cost,
            // rather than a zero that would read as "this one was free".
            span("c", "some-private-model", 1_000_000, 1_000_000, None),
        ])
        .expect("append");

    let (total, derived) = model_cost_all(&store);
    assert!(
        (total - 11.42).abs() < 1e-9,
        "0.42 metered + 11 derived: {total}"
    );
    assert!((derived - 11.0).abs() < 1e-9, "derived share: {derived}");
}

/// Summed over every model row, for the mixed case.
fn model_cost_all(store: &Store) -> (f64, f64) {
    let rows = store
        .llm_aggregate(LlmGroupBy::Model, None, None)
        .expect("aggregate");
    rows.iter().fold((0.0, 0.0), |(total, derived), row| {
        (total + row.cost_usd, derived + row.cost_derived_usd)
    })
}

#[test]
fn a_sealed_rollup_is_not_believed_after_the_rates_change() {
    // The whole reason the pricing fingerprint is part of a sidecar's binding.
    // Seal a segment under one table, reopen under another, and the answer
    // must follow the new rates — not the counters cached under the old ones.
    let dir = test_dir("reprice");

    {
        let store = Store::open(&dir, config(Some(TABLE))).expect("opens");
        store
            .ingest_batch(vec![span("a", "gpt-5.6-sol", 1_000_000, 1_000_000, None)])
            .expect("append");
        // Seal, so the counters are written to a sidecar rather than living
        // in the write buffer where nothing is cached.
        store.flush().expect("flush");
        let (total, _) = model_cost(&store);
        assert!(
            (total - 11.0).abs() < 1e-9,
            "under the first table: {total}"
        );
    }

    // A fresh process: the in-memory cache is empty, so this reads the
    // sidecar — the exact path where a stale rollup would go unnoticed.
    {
        let store = Store::open(&dir, config(Some(REPRICED))).expect("reopens");
        let (total, derived) = model_cost(&store);
        assert!(
            (total - 22.0).abs() < 1e-9,
            "doubled rates must double the cost; got {total}, which is the \
             stale rollup if it is 11"
        );
        assert!((derived - 22.0).abs() < 1e-9, "derived share: {derived}");
    }

    // And back again, to show the gate is not a one-way latch.
    {
        let store = Store::open(&dir, config(Some(TABLE))).expect("reopens");
        let (total, _) = model_cost(&store);
        assert!(
            (total - 11.0).abs() < 1e-9,
            "back under the first table: {total}"
        );
    }
}

#[test]
fn removing_the_table_returns_the_store_to_metered_cost_only() {
    let dir = test_dir("removed");
    {
        let store = Store::open(&dir, config(Some(TABLE))).expect("opens");
        store
            .ingest_batch(vec![span("a", "gpt-5.6-sol", 1_000_000, 1_000_000, None)])
            .expect("append");
        store.flush().expect("flush");
        assert!((model_cost(&store).0 - 11.0).abs() < 1e-9);
    }
    {
        let store = Store::open(&dir, config(None)).expect("reopens");
        assert_eq!(
            model_cost(&store),
            (0.0, 0.0),
            "an operator who removed the table must stop seeing its numbers"
        );
    }
}

#[test]
fn sessions_report_derived_cost_too() {
    // Cost appears on more than one surface, and a surface that missed the
    // pricing hook would disagree with the others while looking fine alone.
    let dir = test_dir("sessions");
    let store = Store::open(&dir, config(Some(TABLE))).expect("opens");
    store
        .ingest_batch(vec![span("a", "gpt-5.6-sol", 1_000_000, 1_000_000, None)])
        .expect("append");

    let sessions = store
        .sessions(None, None, 10, SessionOrder::default())
        .expect("sessions");
    let session = sessions.first().expect("one session");
    assert!(
        (session.cost_usd - 11.0).abs() < 1e-9,
        "{}",
        session.cost_usd
    );
    assert!((session.cost_derived_usd - 11.0).abs() < 1e-9);
}
