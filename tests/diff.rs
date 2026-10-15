//! `GET /v1/diff` against the real binary: identical runs align step for
//! step, and each kind of divergence — input, decision, config, structure — is
//! found at the step where it happens.

mod common;

use common::Server;
use serde_json::{json, Value};

/// 2026-02-01T00:00:00Z: every seeded run is long finished.
const T0: u64 = 1_769_904_000_000_000_000;
const MS: u64 = 1_000_000;

fn span(run: &str, id: &str, start_ms: u64, status: &str, attributes: Value) -> Value {
    let mut attributes = attributes;
    attributes["session.id"] = json!(run);
    json!({
        "trace_id": format!("tr-{run}"), "span_id": id, "name": id, "service": "research-agent",
        "status": status,
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + (start_ms + 50) * MS,
        "attributes": attributes,
    })
}

fn llm(run: &str, id: &str, start_ms: u64, model: &str, prompt: &str, answer: &str) -> Value {
    span(
        run,
        id,
        start_ms,
        "ok",
        json!({
            "gen_ai.operation.name": "chat", "gen_ai.provider.name": "openai",
            "gen_ai.request.model": model,
            "gen_ai.usage.input_tokens": 100, "gen_ai.usage.output_tokens": 20,
            "llm.cost_usd": 0.01,
            "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": prompt,
            "gen_ai.completion.0.role": "assistant", "gen_ai.completion.0.content": answer,
        }),
    )
}

fn tool(run: &str, id: &str, start_ms: u64, name: &str, arguments: Value, status: &str) -> Value {
    span(
        run,
        id,
        start_ms,
        status,
        json!({
            "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
            "gen_ai.tool.call.arguments": serde_json::to_string(&arguments).unwrap(),
            "gen_ai.tool.call.result": "results",
        }),
    )
}

/// The base shape: plan, search, fetch, answer.
fn research(
    run: &str,
    query: &str,
    plan_model: &str,
    answer: &str,
    with_search: bool,
) -> Vec<Value> {
    let mut spans = vec![llm(
        run,
        "1-plan",
        0,
        plan_model,
        "research the question",
        "search then fetch",
    )];
    if with_search {
        spans.push(tool(
            run,
            "2-search",
            100,
            "web_search",
            json!({"q": query}),
            "ok",
        ));
    }
    spans.push(tool(
        run,
        "3-fetch",
        200,
        "fetch_page",
        json!({"url": "https://example.com/a"}),
        "ok",
    ));
    spans.push(llm(
        run,
        "4-answer",
        300,
        "gpt-4o",
        "summarise the page",
        answer,
    ));
    spans
}

fn seeded(server: &Server) {
    let mut spans = Vec::new();
    spans.extend(research(
        "base",
        "rust lcs",
        "gpt-4o",
        "LCS aligns sequences.",
        true,
    ));
    spans.extend(research(
        "twin",
        "rust lcs",
        "gpt-4o",
        "LCS aligns sequences.",
        true,
    ));
    spans.extend(research(
        "input",
        "python lcs",
        "gpt-4o",
        "LCS aligns sequences.",
        true,
    ));
    spans.extend(research(
        "decision",
        "rust lcs",
        "gpt-4o",
        "I could not find it.",
        true,
    ));
    spans.extend(research(
        "config",
        "rust lcs",
        "gpt-4o-mini",
        "LCS aligns sequences.",
        true,
    ));
    spans.extend(research(
        "structure",
        "rust lcs",
        "gpt-4o",
        "LCS aligns sequences.",
        false,
    ));
    // A run that failed: the fetch errored and the agent stopped there.
    spans.push(llm(
        "failed",
        "1-plan",
        0,
        "gpt-4o",
        "research the question",
        "search then fetch",
    ));
    spans.push(tool(
        "failed",
        "2-search",
        100,
        "web_search",
        json!({"q": "rust lcs"}),
        "ok",
    ));
    spans.push(tool(
        "failed",
        "3-fetch",
        200,
        "fetch_page",
        json!({"url": "https://example.com/a"}),
        "error",
    ));
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &[]);
    assert_eq!(status, 200, "{body}");
}

fn diff(server: &Server, a: &str, b: &str) -> Value {
    let (status, body) = server.json("GET", &format!("/v1/diff?a={a}&b={b}"), None, &[]);
    assert_eq!(status, 200, "{body}");
    body
}

#[test]
fn identical_runs_align_step_for_step_with_no_divergence() {
    let server = Server::spawn(&[]);
    seeded(&server);
    let body = diff(&server, "base", "twin");
    assert_eq!(body["first_divergence"], Value::Null, "{body}");
    assert_eq!(body["truncated"], json!(false));
    let aligned = body["aligned"].as_array().unwrap();
    assert_eq!(aligned.len(), 4);
    for (index, entry) in aligned.iter().enumerate() {
        assert_eq!(entry["op"], json!("same"), "{entry}");
        assert_eq!(
            (entry["a"].as_u64(), entry["b"].as_u64()),
            (Some(index as u64), Some(index as u64))
        );
        assert_eq!(entry["fingerprint_a"], entry["fingerprint_b"]);
        assert!(entry.get("why").is_none());
    }
    assert_eq!(
        aligned[1]["fingerprint_a"].as_str().unwrap()[..15],
        *"tool:web_search"
    );
    for side in ["a", "b"] {
        let summary = &body[side];
        assert_eq!(summary["steps"], json!(4));
        assert_eq!(summary["tokens"], json!(240));
        assert!((summary["cost_usd"].as_f64().unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(summary["duration_ns"], json!(350 * MS));
        assert_eq!(summary["errors"], json!(0));
        assert_eq!(summary["outcome"], json!("success"));
    }
    assert_eq!(body["a"]["run"], json!("base"));
    assert_eq!(body["b"]["run"], json!("twin"));
    assert_eq!(
        body["delta"],
        json!({"cost_usd": 0.0, "tokens": 0, "duration_ns": 0, "errors": 0, "steps": 0})
    );
}

#[test]
fn each_divergence_is_found_where_it_happens() {
    let server = Server::spawn(&[]);
    seeded(&server);

    // Input: the search ran with a different query.
    let body = diff(&server, "base", "input");
    let first = &body["first_divergence"];
    assert_eq!(
        (
            first["kind"].as_str(),
            first["a"].as_u64(),
            first["b"].as_u64()
        ),
        (Some("input"), Some(1), Some(1)),
        "{body}"
    );
    assert!(first["explanation"]
        .as_str()
        .unwrap()
        .contains("tool:web_search"));
    assert_eq!(body["aligned"][1]["op"], json!("changed"));
    assert_eq!(body["aligned"][1]["why"], json!("input"));
    assert_eq!(
        body["aligned"][3]["op"],
        json!("same"),
        "the later steps still line up"
    );

    // Decision: the last model call got the same input and answered otherwise.
    let body = diff(&server, "base", "decision");
    let first = &body["first_divergence"];
    assert_eq!(
        (
            first["kind"].as_str(),
            first["a"].as_u64(),
            first["b"].as_u64()
        ),
        (Some("decision"), Some(3), Some(3)),
        "{body}"
    );
    assert!(first["explanation"]
        .as_str()
        .unwrap()
        .contains("same input"));

    // Config: the planning call ran on another model. One changed entry, not
    // a deletion plus an insertion.
    let body = diff(&server, "base", "config");
    let first = &body["first_divergence"];
    assert_eq!(
        (
            first["kind"].as_str(),
            first["a"].as_u64(),
            first["b"].as_u64()
        ),
        (Some("config"), Some(0), Some(0)),
        "{body}"
    );
    let explanation = first["explanation"].as_str().unwrap();
    assert!(explanation.contains("gpt-4o-mini"), "{explanation}");
    assert_eq!(body["aligned"].as_array().unwrap().len(), 4);
    assert_eq!(
        body["aligned"][0]["fingerprint_b"].as_str().unwrap()[..15],
        *"llm:gpt-4o-mini"
    );

    // Structure: the second run skipped the search.
    let body = diff(&server, "base", "structure");
    let first = &body["first_divergence"];
    assert_eq!(
        (
            first["kind"].as_str(),
            first["a"].as_u64(),
            first["b"].as_u64()
        ),
        (Some("structure"), Some(1), Some(1)),
        "{body}"
    );
    let aligned = body["aligned"].as_array().unwrap();
    assert_eq!(aligned[1]["op"], json!("delete"));
    assert_eq!(aligned[1]["b"], Value::Null);
    assert_eq!(aligned[1]["fingerprint_b"], Value::Null);
    assert_eq!(aligned[1]["why"], json!("structure"));
    assert_eq!(
        aligned[2]["op"],
        json!("same"),
        "fetch still aligns: {aligned:?}"
    );
    assert_eq!(body["delta"]["steps"], json!(-1));

    // And in the other direction it is an insertion.
    let body = diff(&server, "structure", "base");
    assert_eq!(body["aligned"][1]["op"], json!("insert"));
    assert_eq!(body["first_divergence"]["a"], json!(1));

    // A failed run: the outcome, the error count and the missing answer.
    let body = diff(&server, "base", "failed");
    assert_eq!(body["b"]["outcome"], json!("failure"));
    assert_eq!(body["b"]["errors"], json!(1));
    assert_eq!(body["delta"]["errors"], json!(1));
    assert_eq!(body["first_divergence"]["kind"], json!("structure"));
    assert_eq!(body["first_divergence"]["a"], json!(3));
    assert_eq!(body["first_divergence"]["b"], json!(3), "past the end of b");
}

#[test]
fn diff_refuses_missing_runs_and_malformed_requests() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "ro:op-token,ro@beta:beta-token")]);
    let op = [("Authorization", "Bearer op-token")];
    let beta = [("Authorization", "Bearer beta-token")];
    let (status, _) = server.json("GET", "/v1/diff?a=base&b=twin", None, &op);
    assert_eq!(status, 404, "nothing is seeded yet");

    let server = Server::spawn(&[]);
    seeded(&server);
    let (status, body) = server.json("GET", "/v1/diff?a=base&b=ghost", None, &[]);
    assert_eq!(
        (status, body),
        (404, json!({"error": "run not found: ghost"}))
    );
    let (status, body) = server.json("GET", "/v1/diff?a=base", None, &[]);
    assert_eq!(status, 400, "{body}");
    let (status, body) = server.json("GET", "/v1/diff?a=base&b=twin&c=x", None, &[]);
    assert_eq!(status, 400, "{body}");
    // A trace id names a run too.
    let body = diff(&server, "trace:tr-base", "trace:tr-twin");
    assert_eq!(body["first_divergence"], Value::Null);

    // A bound credential reads inside its tenant and may not name another.
    let bound = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "rw:op-token,ro@beta:beta-token")]);
    let mut spans: Vec<Value> = research("base", "q", "gpt-4o", "a", true);
    spans.extend(research("twin", "q", "gpt-4o", "a", true));
    let (status, body) = bound.json("POST", "/v1/spans", Some(&json!(spans)), &op);
    assert_eq!(status, 200, "{body}");
    let (status, _) = bound.json("GET", "/v1/diff?a=base&b=twin", None, &op);
    assert_eq!(status, 200);
    let (status, body) = bound.json("GET", "/v1/diff?a=base&b=twin", None, &beta);
    assert_eq!(
        (status, body),
        (404, json!({"error": "run not found: base"}))
    );
    let (status, _) = bound.json("GET", "/v1/diff?a=base&b=twin&tenant=", None, &beta);
    assert_eq!(status, 403);
}
