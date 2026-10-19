//! `GET /v1/recall` against the real binary: earlier runs found by the words
//! they contain, successful ones first, filtered by outcome, each summarised
//! by a run-length-compressed skeleton.

mod common;

use common::Server;
use serde_json::{json, Value};

/// 2026-03-01T00:00:00Z: every seeded run is long finished.
const T0: u64 = 1_772_323_200_000_000_000;
const MS: u64 = 1_000_000;
const HOUR_MS: u64 = 3_600_000;

fn span(run: &str, id: &str, start_ms: u64, status: &str, attributes: Value) -> Value {
    let mut attributes = attributes;
    attributes["session.id"] = json!(run);
    json!({
        "trace_id": format!("tr-{run}"), "span_id": id, "name": id, "service": "support-agent",
        "status": status,
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + (start_ms + 20) * MS,
        "attributes": attributes,
    })
}

fn llm(run: &str, id: &str, start_ms: u64, prompt: &str) -> Value {
    span(
        run,
        id,
        start_ms,
        "ok",
        json!({
            "gen_ai.operation.name": "chat", "gen_ai.request.model": "gpt-4o",
            "llm.cost_usd": 0.002,
            "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": prompt,
            "gen_ai.completion.0.content": "working on it",
        }),
    )
}

fn tool(run: &str, id: &str, start_ms: u64, name: &str, status: &str) -> Value {
    span(
        run,
        id,
        start_ms,
        status,
        json!({
            "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
            "gen_ai.tool.call.arguments": "{\"ticket\": 1}",
        }),
    )
}

fn seeded(server: &Server) {
    let mut spans = vec![
        // The newest run, with the MOST matching spans, failed: its refund
        // call errored.
        llm(
            "failed-refund",
            "a",
            5 * HOUR_MS,
            "customer wants a refund for order 7",
        ),
        llm(
            "failed-refund",
            "b",
            5 * HOUR_MS + 100,
            "retry the refund for order 7",
        ),
        llm(
            "failed-refund",
            "c",
            5 * HOUR_MS + 200,
            "refund order 7 one more time",
        ),
        tool(
            "failed-refund",
            "d",
            5 * HOUR_MS + 300,
            "issue_refund",
            "error",
        ),
        // An older run that succeeded, with one matching span; it declared
        // its goal and outcome.
        llm(
            "good-refund",
            "a",
            HOUR_MS,
            "customer wants a refund for order 9",
        ),
        tool("good-refund", "b", HOUR_MS + 100, "lookup_order", "ok"),
        tool("good-refund", "c", HOUR_MS + 200, "lookup_order", "ok"),
        tool("good-refund", "d", HOUR_MS + 300, "lookup_order", "ok"),
        tool("good-refund", "e", HOUR_MS + 400, "issue_refund", "ok"),
        // A second success, older still, also with one match.
        llm("older-refund", "a", 10, "process a refund for order 3"),
        tool("older-refund", "b", 110, "issue_refund", "ok"),
        // Unrelated.
        llm("shipping", "a", 2 * HOUR_MS, "where is my parcel"),
    ];
    spans[4]["attributes"]["session.goal"] = json!("refund order 9");
    spans[8]["attributes"]["session.outcome"] = json!("success");
    // A long run whose skeleton must be cut: 50 alternating steps, all
    // matching nothing but the first.
    spans.push(llm(
        "long-refund",
        "000",
        3 * HOUR_MS,
        "refund order 1 slowly",
    ));
    for index in 1..50u64 {
        let id = format!("{index:03}");
        if index % 2 == 0 {
            spans.push(llm("long-refund", &id, 3 * HOUR_MS + index * 30, "think"));
        } else {
            spans.push(tool(
                "long-refund",
                &id,
                3 * HOUR_MS + index * 30,
                "check",
                "ok",
            ));
        }
    }
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &[]);
    assert_eq!(status, 200, "{body}");
}

fn recall(server: &Server, query: &str) -> Value {
    let (status, body) = server.json("GET", &format!("/v1/recall?{query}"), None, &[]);
    assert_eq!(status, 200, "{body}");
    body
}

fn runs(body: &Value) -> Vec<&str> {
    body["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|run| run["run"].as_str().unwrap())
        .collect()
}

#[test]
fn successful_runs_rank_first_then_by_matches_then_recency() {
    let server = Server::spawn(&[]);
    seeded(&server);
    let body = recall(&server, "q=refund+order");
    // The three successes first: good-refund matches on four spans (its
    // prompt, and three `lookup_order` calls inside a run whose id says
    // refund), then the one-match runs by recency. The failed run comes last
    // despite its three matches and its newer activity.
    assert_eq!(
        runs(&body),
        [
            "good-refund",
            "long-refund",
            "older-refund",
            "failed-refund"
        ]
    );
    assert_eq!(body["truncated"], json!(false));

    let good = &body["runs"][0];
    assert_eq!(good["outcome"], json!("success"));
    assert_eq!(good["outcome_source"], json!("declared"));
    assert_eq!(good["goal"], json!("refund order 9"));
    assert_eq!(good["score"], json!(4));
    assert_eq!(good["steps"], json!(5));
    assert_eq!(good["tenant"], json!(""));
    assert_eq!(good["last_end_ns"], json!(T0 + (HOUR_MS + 420) * MS));
    assert!((good["cost_usd"].as_f64().unwrap() - 0.002).abs() < 1e-12);
    assert!(good.get("cause").is_none(), "a success names no cause");

    let failed = &body["runs"][3];
    assert_eq!(failed["outcome"], json!("failure"));
    assert_eq!(failed["outcome_source"], json!("derived"));
    assert_eq!(failed["score"], json!(3));
    assert_eq!(
        failed["cause"],
        json!({"trace_id": "tr-failed-refund", "span_id": "d", "name": "d", "rule": "leaf_error"})
    );

    let limited = recall(&server, "q=refund+order&limit=2");
    assert_eq!(runs(&limited), ["good-refund", "long-refund"]);
}

#[test]
fn the_outcome_filter_returns_only_that_outcome() {
    let server = Server::spawn(&[]);
    seeded(&server);
    assert_eq!(
        runs(&recall(&server, "q=refund&outcome=failure")),
        ["failed-refund"]
    );
    let successes = recall(&server, "q=refund&outcome=success");
    let names = runs(&successes);
    assert_eq!(names.len(), 3, "{successes}");
    assert!(!names.contains(&"failed-refund"));
    // Among successes, more matching spans rank first.
    let any = recall(&server, "q=refund&outcome=any");
    assert_eq!(runs(&any).last(), Some(&"failed-refund"));
    assert!(runs(&recall(&server, "q=parcel&outcome=failure")).is_empty());
}

#[test]
fn skeletons_fold_repeated_steps_and_cap_their_length() {
    let server = Server::spawn(&[]);
    seeded(&server);
    let body = recall(&server, "q=refund+order&outcome=success");
    let by_run = |name: &str| {
        body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["run"] == json!(name))
            .unwrap_or_else(|| panic!("{name} missing from {body}"))
            .clone()
    };
    let good = by_run("good-refund");
    assert_eq!(
        good["skeleton"],
        json!(["llm:gpt-4o", "tool:lookup_order x3", "tool:issue_refund"])
    );
    assert_eq!(good["skeleton_truncated"], json!(false));
    let long = by_run("long-refund");
    assert_eq!(long["steps"], json!(50));
    assert_eq!(long["skeleton"].as_array().unwrap().len(), 40);
    assert_eq!(long["skeleton_truncated"], json!(true));
    assert_eq!(long["skeleton"][0], json!("llm:gpt-4o"));
    assert_eq!(long["skeleton"][1], json!("tool:check"));
}

#[test]
fn recall_refuses_what_it_cannot_answer_and_stays_inside_a_tenant() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "rw:op-token,ro@beta:beta-token")]);
    let op = [("Authorization", "Bearer op-token")];
    let beta = [("Authorization", "Bearer beta-token")];
    let spans = vec![llm("default-run", "a", 0, "refund order 1")];
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &op);
    assert_eq!(status, 200, "{body}");

    for (query, needle) in [
        ("", "q is required"),
        ("q=", "q is required"),
        ("q=%3F%21", "no searchable words"),
        ("q=refund&outcome=sometimes", "outcome must be"),
        ("q=refund&limit=0", "limit must be"),
        ("q=refund&bogus=1", "unknown query parameter"),
    ] {
        let (status, body) = server.json("GET", &format!("/v1/recall?{query}"), None, &op);
        assert_eq!(status, 400, "{query}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(needle),
            "{query}: {body}"
        );
    }

    let (status, body) = server.json("GET", "/v1/recall?q=refund", None, &op);
    assert_eq!(status, 200);
    assert_eq!(runs(&body), ["default-run"]);
    // The bound credential sees its own tenant only, and may not name another.
    let (status, body) = server.json("GET", "/v1/recall?q=refund", None, &beta);
    assert_eq!(status, 200);
    assert!(runs(&body).is_empty(), "{body}");
    let (status, _) = server.json("GET", "/v1/recall?q=refund&tenant=", None, &beta);
    assert_eq!(status, 403);
}
