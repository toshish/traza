//! `GET /v1/xray/{trace_id}/{span_id}`, `GET /v1/cache/{run}` and the MCP
//! `xray_step` tool against the real binary: a coding agent's context window
//! read chunk by chunk with provenance, staleness, duplicates and compaction
//! loss; its prompt cache autopsied down to the volatile token and the money;
//! refusals, bounds and tenant isolation.

mod common;

use common::Server;
use serde_json::{json, Value};

/// 2026-02-01T00:00:00Z: every seeded run is long finished.
const T0: u64 = 1_769_904_000_000_000_000;
const MS: u64 = 1_000_000;

const OPERATOR: &str = "operator-token";
const ACME: &str = "acme-reader-token";
const BETA: &str = "beta-reader-token";
const ACME_WRITER: &str = "acme-writer-token";
const BETA_WRITER: &str = "beta-writer-token";

fn tokens() -> String {
    format!(
        "admin:{OPERATOR},ro@acme:{ACME},ro@beta:{BETA},rw@acme:{ACME_WRITER},rw@beta:{BETA_WRITER}"
    )
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

fn get(server: &Server, path: &str, token: Option<&str>) -> (u16, Value) {
    let header = token.map(auth);
    let headers: Vec<(&str, &str)> = header
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .collect();
    server.json("GET", path, None, &headers)
}

fn ingest(server: &Server, spans: Vec<Value>, token: Option<&str>) {
    let header = token.map(auth);
    let headers: Vec<(&str, &str)> = header
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .collect();
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &headers);
    assert_eq!(status, 200, "{body}");
}

fn span(run: &str, id: &str, start_ms: u64, attributes: Value) -> Value {
    let mut attributes = attributes;
    attributes["session.id"] = json!(run);
    json!({
        "trace_id": format!("tr-{run}"), "span_id": id, "name": id, "service": "coder",
        "status": "ok",
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + (start_ms + 50) * MS,
        "attributes": attributes,
    })
}

/// An Anthropic-instrumented model call: messages in Anthropic's content
/// parts, usage in Anthropic's exclusive convention.
fn claude(
    run: &str,
    id: &str,
    start_ms: u64,
    system: &str,
    messages: Value,
    output: Value,
) -> Value {
    span(
        run,
        id,
        start_ms,
        json!({
            "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
            "gen_ai.request.model": "claude-sonnet-x",
            "gen_ai.system_instructions": system,
            "gen_ai.input.messages": messages,
            "gen_ai.output.messages": output,
            "gen_ai.usage.input_tokens": 1200, "gen_ai.usage.output_tokens": 40,
            "gen_ai.usage.cache_read.input_tokens": 0,
        }),
    )
}

fn tool(
    run: &str,
    id: &str,
    start_ms: u64,
    name: &str,
    call: &str,
    args: Value,
    result: &str,
) -> Value {
    span(
        run,
        id,
        start_ms,
        json!({
            "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
            "gen_ai.tool.call.id": call,
            "gen_ai.tool.call.arguments": args.to_string(),
            "gen_ai.tool.call.result": result,
        }),
    )
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]})
}

fn tool_result(id: &str, text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
                                        "content": [{"type": "text", "text": text}]}]})
}

fn said(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

const RULE: &str = "Never edit files under migrations/. Fix the failing parser test.";
const READ: &str = "def parse(value):\n    return zeta_marker_one(value)\n";
const GREP: &str = "tests/test_parser.py:9: assert omega_marker_two(parse('x'))";

fn system_at(time: &str) -> String {
    format!("You are a coding agent. Current time: 2026-02-01T{time}Z. Work carefully.")
}

/// A coding agent: read a file, grep, comment on the grep result, run a
/// no-op, look again (with the grep output pasted twice), then a compacted
/// call that lost the original instruction. The system prompt embeds the
/// clock, which changes between the first two calls.
fn coding_run(run: &str) -> Vec<Value> {
    let turns = [
        user(RULE),
        tool_use("tu1", "read_file", json!({"path": "parser.py"})),
        tool_result("tu1", READ),
        tool_use("tu2", "grep", json!({"pattern": "callers"})),
        tool_result("tu2", GREP),
        said("The omega_marker_two assertion is what fails"),
        tool_use("tu3", "noop", json!({})),
        tool_result("tu3", "ok"),
        user(GREP),
    ];
    let upto = |n: usize| Value::Array(turns[..n].to_vec());
    let early = system_at("09:14:03");
    let late = system_at("09:19:44");
    vec![
        claude(run, "m0", 0, &early, upto(1), json!([turns[1].clone()])),
        tool(
            run,
            "x1",
            100,
            "read_file",
            "tu1",
            json!({"path": "parser.py"}),
            READ,
        ),
        claude(run, "m2", 200, &late, upto(3), json!([turns[3].clone()])),
        tool(
            run,
            "x3",
            300,
            "grep",
            "tu2",
            json!({"pattern": "callers"}),
            GREP,
        ),
        claude(run, "m4", 400, &late, upto(5), json!([turns[5].clone()])),
        tool(run, "x5", 500, "noop", "tu3", json!({}), "ok"),
        claude(run, "m6", 600, &late, upto(9), json!([said("Done.")])),
        claude(
            run,
            "m7",
            700,
            &late,
            json!([user(
                "Summary so far: the parser test fails on omega_marker_two."
            )]),
            json!([said("Continuing.")]),
        ),
    ]
}

fn pricing_file() -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "traza-xray-pricing-{}-{nonce}.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"{"models": {"claude-sonnet-*": {"input_per_mtok": 3.0, "output_per_mtok": 15.0,
                                           "cache_read_per_mtok": 0.3,
                                           "cache_write_per_mtok": 3.75}}}"#,
    )
    .expect("writes the pricing file");
    path
}

#[test]
fn the_xray_shows_provenance_staleness_duplicates_and_compaction_loss() {
    let server = Server::spawn(&["--xray-stale-steps", "2"]);
    ingest(&server, coding_run("fix-1"), None);

    let (status, xray) = get(&server, "/v1/xray/tr-fix-1/m6", None);
    assert_eq!(status, 200, "{xray}");
    assert_eq!(xray["run"], json!("fix-1"));
    assert_eq!(xray["step_index"], json!(6));
    assert_eq!(xray["model"], json!("claude-sonnet-x"));
    assert_eq!(
        xray["stale_after_steps"],
        json!(2),
        "the flag sets the default"
    );
    let chunks = xray["chunks"].as_array().expect("chunks");
    let kinds: Vec<&str> = chunks
        .iter()
        .map(|chunk| chunk["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "system",
            "user",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "assistant",
            "tool_call",
            "tool_result",
            "user"
        ],
        "{xray}"
    );
    // Provenance: the system prompt from the first call that carried this
    // exact text (the clock changed after step 0); each result from its
    // tool step; the model's words from the call that said them.
    let from = |index: usize| &chunks[index]["introduced_by"];
    assert_eq!(from(0)["step_index"], json!(2));
    assert_eq!(from(1)["step_index"], json!(0));
    assert_eq!(from(3)["step_index"], json!(1));
    assert_eq!(from(3)["kind"], json!("tool"));
    assert_eq!(from(3)["name"], json!("read_file"));
    assert_eq!(from(3)["via"], json!("content"));
    assert_eq!(from(6)["step_index"], json!(4));
    assert_eq!(chunks[3]["age_steps"], json!(5));
    // The read_file result was never drawn on; the grep result was quoted.
    assert_eq!(chunks[3]["stale"], json!(true));
    assert_eq!(chunks[5]["stale"], json!(false));
    assert_eq!(xray["totals"]["stale_chunks"], json!(1));
    // The grep output pasted again is a duplicate.
    assert_eq!(xray["duplicates"][0]["indices"], json!([5, 9]));
    assert_eq!(chunks[9]["duplicate_of"], json!(5));
    // Provider numbers sit beside the estimates.
    assert_eq!(xray["totals"]["reported_input_tokens"], json!(1200));
    assert_eq!(xray["totals"]["cache_read"], json!(0));
    assert!(xray["totals"]["est_tokens"].as_u64().unwrap() > 0);
    let sources: Vec<&str> = xray["composition"]["by_source"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["key"].as_str().unwrap())
        .collect();
    assert!(
        sources.contains(&"tool:read_file") && sources.contains(&"tool:grep"),
        "{sources:?}"
    );

    // A per-request threshold overrides the flag.
    let (status, lax) = get(&server, "/v1/xray/tr-fix-1/m6?stale_steps=50", None);
    assert_eq!(status, 200);
    assert_eq!(lax["totals"]["stale_chunks"], json!(0));

    // The compacted call lost the instruction, and says so.
    let (status, compacted) = get(&server, "/v1/xray/tr-fix-1/m7", None);
    assert_eq!(status, 200, "{compacted}");
    assert_eq!(compacted["previous"]["step_index"], json!(6));
    let dropped = compacted["dropped_since_previous"].as_array().unwrap();
    let rule = dropped
        .iter()
        .find(|entry| entry["preview"] == json!(RULE))
        .unwrap_or_else(|| panic!("the instruction is reported lost: {compacted}"));
    assert_eq!(rule["kind"], json!("instruction"));
    assert_eq!(rule["introduced_by"]["step_index"], json!(0));
    assert_eq!(compacted["cache"]["cause"], json!("new_context"));
    assert_eq!(
        compacted["cache"]["first_divergent_chunk"]["change"],
        json!("rewritten")
    );
}

#[test]
fn the_cache_autopsy_names_the_volatile_token_and_prices_the_loss() {
    let pricing = pricing_file();
    let server = Server::spawn(&["--pricing", pricing.to_str().unwrap()]);
    ingest(&server, coding_run("fix-2"), None);

    let (status, autopsy) = get(&server, "/v1/cache/fix-2", None);
    assert_eq!(status, 200, "{autopsy}");
    let steps = autopsy["steps"].as_array().expect("steps");
    let causes: Vec<&str> = steps
        .iter()
        .map(|step| step["cause"].as_str().unwrap())
        .collect();
    assert_eq!(
        causes,
        [
            "new_context",
            "volatile_content",
            "stable",
            "stable",
            "new_context"
        ],
        "{autopsy}"
    );
    let edit = &steps[1];
    assert_eq!(edit["step_index"], json!(2));
    assert_eq!(edit["previous_step_index"], json!(0));
    let divergence = &edit["first_divergent_chunk"];
    assert_eq!(divergence["index"], json!(0));
    assert_eq!(divergence["role"], json!("system"));
    assert_eq!(divergence["change"], json!("edited"));
    assert_eq!(divergence["volatile"], json!("timestamp"));
    assert_eq!(divergence["diff"]["before"], json!("2026-02-01T09:14:03Z"));
    assert_eq!(divergence["diff"]["after"], json!("2026-02-01T09:19:44Z"));
    assert_eq!(
        divergence["diff"]["offset"],
        json!("You are a coding agent. Current time: ".len())
    );
    assert!(edit["est_tokens_lost"].as_u64().unwrap() > 0);
    let cost = edit["est_cost_lost_usd"].as_f64().expect("priced");
    assert!(cost > 0.0);
    // Stable prefixes the provider reported no read for are disagreements.
    let summary = &autopsy["summary"];
    assert_eq!(summary["steps"], json!(5));
    assert_eq!(summary["breaks"], json!(2));
    assert_eq!(summary["by_cause"]["volatile_content"], json!(1));
    assert_eq!(summary["disagreements"], json!(2));
    assert!(summary["disagreement_advice"].is_string(), "{summary}");
    let culprits = summary["culprits"].as_array().unwrap();
    assert!(
        culprits
            .iter()
            .any(|culprit| culprit["volatile"] == json!("timestamp")
                && culprit["role"] == json!("system")
                && culprit["advice"]
                    .as_str()
                    .is_some_and(|advice| advice.contains("clock value"))),
        "{summary}"
    );
    assert!(summary["est_cost_lost_usd"].as_f64().unwrap() >= cost);

    // The X-Ray carries the same verdict for its call.
    let (_, xray) = get(&server, "/v1/xray/tr-fix-2/m2", None);
    assert_eq!(xray["cache"]["cause"], json!("volatile_content"));
    assert_eq!(
        xray["cache"]["est_cost_lost_usd"],
        edit["est_cost_lost_usd"]
    );
    // The explanation quotes the calibrated figure the step reports.
    let lost = edit["est_tokens_lost"].as_u64().unwrap();
    assert!(
        edit["explanation"]
            .as_str()
            .unwrap()
            .contains(&format!("~{lost} tokens")),
        "{edit}"
    );
    // The re-stamped system prompt is an edit, not a lost instruction.
    assert_eq!(xray["dropped_since_previous"], json!([]), "{xray}");
    let edited = &xray["edited_since_previous"][0];
    assert_eq!(edited["index"], json!(0));
    assert_eq!(edited["previous_index"], json!(0));
    assert_eq!(edited["role"], json!("system"));
    assert_eq!(edited["volatile"], json!("timestamp"));
    assert_eq!(edited["diff"]["after"], json!("2026-02-01T09:19:44Z"));

    // A ttl too short for the gaps turns stable calls into expiries.
    let (status, strict) = get(&server, "/v1/cache/fix-2?ttl_seconds=1", None);
    assert_eq!(status, 200);
    let _ = std::fs::remove_file(&pricing);
    let strict_causes: Vec<&str> = strict["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["cause"].as_str().unwrap())
        .collect();
    // The calls are 200 ms apart: a one-second TTL still holds.
    assert_eq!(strict_causes[2], "stable", "{strict}");
    assert_eq!(strict["ttl_seconds"], json!(1));
}

#[test]
fn offloaded_prompts_are_resolved_before_they_are_chunked() {
    let server = Server::spawn(&["--payload-threshold-bytes", "64"]);
    let long = "a long system rule ".repeat(10);
    let mut call = claude("big", "m0", 0, &long, json!(null), json!([said("ok")]));
    call["attributes"]["gen_ai.input.messages"] =
        json!(json!([user(&"question ".repeat(20))]).to_string());
    ingest(&server, vec![call], None);
    let (status, xray) = get(&server, "/v1/xray/tr-big/m0", None);
    assert_eq!(status, 200, "{xray}");
    let kinds: Vec<&str> = xray["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| chunk["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["system", "user"], "{xray}");
    assert_eq!(xray["resolved_partial"], json!(false));
    let (_, opaque) = get(&server, "/v1/xray/tr-big/m0?resolve=false", None);
    assert_eq!(opaque["resolved_partial"], json!(true));
    assert_eq!(opaque["cache"]["cause"], json!("unknown"));
}

#[test]
fn bad_requests_are_refused_precisely() {
    let server = Server::spawn(&[]);
    ingest(&server, coding_run("fix-3"), None);
    for (path, status, needle) in [
        (
            "/v1/xray/tr-fix-3/m0?bogus=1",
            400,
            "unknown query parameter: bogus",
        ),
        (
            "/v1/xray/tr-fix-3/m0?stale_steps=0",
            400,
            "stale_steps must be",
        ),
        ("/v1/xray/tr-fix-3/m0?resolve=maybe", 400, "resolve must be"),
        (
            "/v1/xray/tr-fix-3/m0?ttl_seconds=999999",
            400,
            "ttl_seconds must be",
        ),
        ("/v1/xray/tr-fix-3/x1", 400, "tool step"),
        ("/v1/xray/tr-fix-3/nope", 404, "span not found"),
        ("/v1/xray/tr-fix-3", 400, "expected /v1/xray/"),
        ("/v1/xray/tr-fix-3/m0/extra", 400, "expected /v1/xray/"),
        ("/v1/cache/no-such-run", 404, "run not found"),
        ("/v1/cache/fix-3?tenant=Not%20Valid", 400, "tenant must be"),
    ] {
        let (code, body) = get(&server, path, None);
        assert_eq!(code, status, "{path}: {body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains(needle),
            "{path}: {body}"
        );
    }
    let (status, _) = server.json("POST", "/v1/cache/fix-3", Some(&json!({})), &[]);
    assert_eq!(status, 404, "the routes are reads");
}

#[test]
fn tenants_see_only_their_own_contexts() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", &tokens())]);
    ingest(&server, coding_run("shared-id"), Some(ACME_WRITER));
    // Beta reuses the very same trace, span and run ids with other text.
    let mut beta = coding_run("shared-id");
    beta[0]["attributes"]["gen_ai.system_instructions"] = json!("beta's own system prompt");
    ingest(&server, beta, Some(BETA_WRITER));

    let (status, acme) = get(&server, "/v1/xray/tr-shared-id/m0", Some(ACME));
    assert_eq!(status, 200, "{acme}");
    assert_eq!(acme["tenant"], json!("acme"));
    assert!(acme["chunks"][0]["preview"]
        .as_str()
        .unwrap()
        .starts_with("You are a coding agent."));
    let (status, beta) = get(&server, "/v1/xray/tr-shared-id/m0", Some(BETA));
    assert_eq!(status, 200);
    assert_eq!(
        beta["chunks"][0]["preview"],
        json!("beta's own system prompt")
    );

    // Naming another tenant is refused with what would have worked.
    let (status, body) = get(&server, "/v1/xray/tr-shared-id/m0?tenant=beta", Some(ACME));
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["required"], json!("unbound"));
    let (status, body) = get(&server, "/v1/cache/shared-id?tenant=acme", Some(BETA));
    assert_eq!(status, 403, "{body}");

    // The operator view must say whose span it means.
    let (status, body) = get(&server, "/v1/xray/tr-shared-id/m0", Some(OPERATOR));
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("2 tenants"),
        "{body}"
    );
    let (status, chosen) = get(
        &server,
        "/v1/xray/tr-shared-id/m0?tenant=beta",
        Some(OPERATOR),
    );
    assert_eq!(status, 200);
    assert_eq!(chosen["tenant"], json!("beta"));

    // A run only acme has is invisible to beta.
    ingest(&server, coding_run("acme-only"), Some(ACME_WRITER));
    assert_eq!(get(&server, "/v1/cache/acme-only", Some(BETA)).0, 404);
    assert_eq!(get(&server, "/v1/xray/tr-acme-only/m0", Some(BETA)).0, 404);
    let (status, autopsy) = get(&server, "/v1/cache/acme-only", Some(ACME));
    assert_eq!(status, 200);
    assert_eq!(autopsy["tenant"], json!("acme"));
    assert_eq!(autopsy["summary"]["steps"], json!(5));
}

#[test]
fn the_mcp_tool_renders_a_framed_compact_xray() {
    let server = Server::spawn(&["--mcp"]);
    ingest(&server, coding_run("fix-4"), None);
    let rpc = |arguments: Value| -> Value {
        let (status, body) = server.json(
            "POST",
            "/v1/mcp",
            Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": "xray_step", "arguments": arguments}})),
            &[("Accept", "application/json, text/event-stream")],
        );
        assert_eq!(status, 200, "{body}");
        body["result"].clone()
    };
    let result = rpc(json!({"run_id": "fix-4", "step": 7}));
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("X-Ray of step 7: 2 chunks"), "{text}");
    assert!(text.contains("of them INSTRUCTIONS"), "{text}");
    let open = text
        .find("<traza:telemetry untrusted=\"true\">")
        .expect("framed");
    let rule = text
        .find("Never edit files under migrations/")
        .expect("the lost rule");
    assert!(open < rule, "stored text is inside the block: {text}");
    let refused = rpc(json!({"trace_id": "tr-fix-4", "span_id": "x1"}));
    assert_eq!(refused["isError"], json!(true));
}

#[test]
fn a_zero_stale_threshold_is_refused_at_startup() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .args(["--xray-stale-steps", "0", "--port", "0"])
        .env_remove("TRAZA_TOKENS")
        .output()
        .expect("runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--xray-stale-steps must be at least 1"),
        "{stderr}"
    );
}
