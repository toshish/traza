//! Unit tests for the X-Ray and the Cache Autopsy over a real store.

use serde_json::{json, Value};

use super::*;
use crate::tape::tests::{span, TempStore};

const SECOND: u64 = 1_000_000_000;

fn llm(run: &str, id: &str, start: u64, input: Value, output: Value) -> Span {
    span(
        &format!("tr-{run}"),
        id,
        start,
        json!({
            "session.id": run,
            "gen_ai.operation.name": "chat",
            "gen_ai.provider.name": "anthropic",
            "gen_ai.request.model": "claude-x",
            "gen_ai.input.messages": input,
            "gen_ai.output.messages": output,
        }),
    )
}

fn tool(
    run: &str,
    id: &str,
    start: u64,
    name: &str,
    call: &str,
    args: Value,
    result: &str,
) -> Span {
    span(
        &format!("tr-{run}"),
        id,
        start,
        json!({
            "session.id": run,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": name,
            "gen_ai.tool.call.id": call,
            "gen_ai.tool.call.arguments": serde_json::to_string(&args).unwrap(),
            "gen_ai.tool.call.result": result,
        }),
    )
}

fn msg(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": id, "type": "function",
         "function": {"name": name, "arguments": serde_json::to_string(&args).unwrap()}}
    ]})
}

fn result(id: &str, text: &str) -> Value {
    json!({"role": "tool", "tool_call_id": id, "content": text})
}

const SYSTEM: &str = "You are a careful coding agent.";
const TASK: &str = "Fix the parser bug";
const READ: &str = "fn parse_config() {\n  let zeta_marker_one = load();\n}";
const GREP: &str = "parser.rs:2: let omega_marker_two = load();";

/// An agent run: read a file, grep, say something about the grep result
/// only, call a no-op, and look again — with the grep result pasted twice.
fn agent_run() -> Vec<Span> {
    let history = |upto: usize| -> Vec<Value> {
        let all = vec![
            msg("system", SYSTEM),
            msg("user", TASK),
            call("c1", "read_file", json!({"path": "parser.rs"})),
            result("c1", READ),
            call("c2", "grep", json!({"q": "callers"})),
            result("c2", GREP),
            msg("assistant", "The omega_marker_two line is the culprit"),
            call("c3", "noop", json!({})),
            result("c3", "ok"),
            msg("user", GREP),
        ];
        all[..upto].to_vec()
    };
    vec![
        llm(
            "s1",
            "l0",
            100,
            Value::Array(history(2)),
            json!([call("c1", "read_file", json!({"path": "parser.rs"}))]),
        ),
        tool(
            "s1",
            "t1",
            200,
            "read_file",
            "c1",
            json!({"path": "parser.rs"}),
            READ,
        ),
        llm(
            "s1",
            "l2",
            300,
            Value::Array(history(4)),
            json!([call("c2", "grep", json!({"q": "callers"}))]),
        ),
        tool("s1", "t3", 400, "grep", "c2", json!({"q": "callers"}), GREP),
        llm(
            "s1",
            "l4",
            500,
            Value::Array(history(6)),
            json!([msg("assistant", "The omega_marker_two line is the culprit")]),
        ),
        tool("s1", "t5", 600, "noop", "c3", json!({}), "ok"),
        llm(
            "s1",
            "l6",
            700,
            Value::Array(history(10)),
            json!([msg("assistant", "Done.")]),
        ),
    ]
}

fn options(stale_steps: usize) -> XrayOptions {
    XrayOptions {
        stale_steps,
        ..XrayOptions::default()
    }
}

fn xray_of(
    store: &Store,
    tenant: Option<&str>,
    trace: &str,
    span: &str,
    opts: XrayOptions,
) -> XRay {
    store
        .xray(tenant, trace, span, opts)
        .expect("reads")
        .expect("found")
}

#[test]
fn provenance_age_staleness_duplicates_and_composition() {
    let fixture = TempStore::open("xray-provenance", crate::Config::default());
    fixture.store().ingest_batch(agent_run()).expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-s1", "l6", options(2));
    assert_eq!(xray.run, "s1");
    assert_eq!(xray.step_index, Some(6));
    let roles: Vec<(&str, ChunkKind)> = xray
        .chunks
        .iter()
        .map(|chunk| (chunk.role.as_deref().unwrap_or("-"), chunk.kind))
        .collect();
    assert_eq!(roles.len(), 10, "{roles:?}");
    let by = |index: usize| &xray.chunks[index];
    let from = |index: usize| by(index).introduced_by.clone().expect("introduced");

    // The system prompt and the task came in with the first call.
    assert_eq!(from(0).step_index, 0);
    assert_eq!(from(0).kind, StepKind::Llm);
    assert_eq!(by(0).age_steps, Some(6));
    assert_eq!(from(1).step_index, 0);
    // The model's tool call was introduced by the call that made it.
    assert_eq!(by(2).kind, ChunkKind::ToolCall);
    assert_eq!(from(2).step_index, 0);
    // Each tool result by the tool step that returned it.
    assert_eq!(by(3).kind, ChunkKind::ToolResult);
    assert_eq!(from(3).step_index, 1);
    assert_eq!(from(3).kind, StepKind::Tool);
    assert_eq!(from(3).name, "read_file");
    assert_eq!(from(3).via, "content");
    assert_eq!(from(5).step_index, 3);
    assert_eq!(
        from(6).step_index,
        4,
        "the model's own words, from the call that said them"
    );

    // The read_file result is old and nothing the model said since quotes
    // it; the grep result is as old but was quoted ("omega_marker_two").
    assert!(by(3).stale, "{:?}", by(3));
    assert!(!by(5).stale, "{:?}", by(5));
    // Young results are never stale; nor is anything that is not a result.
    assert!(!by(8).stale && !by(0).stale);
    assert_eq!(xray.totals.stale_chunks, 1);
    assert_eq!(xray.totals.stale_est_tokens, by(3).est_tokens);
    // With a laxer threshold nothing is stale.
    let lax = xray_of(fixture.store(), Some(""), "tr-s1", "l6", options(20));
    assert_eq!(lax.totals.stale_chunks, 0);
    assert_eq!(lax.stale_after_steps, 20);

    // The grep output pasted again as a user message is a duplicate.
    assert_eq!(xray.duplicates.len(), 1, "{:?}", xray.duplicates);
    assert_eq!(xray.duplicates[0].indices, vec![5, 9]);
    assert_eq!(by(9).duplicate_of, Some(5));
    assert_eq!(xray.totals.duplicate_est_tokens, by(9).est_tokens);

    // Composition names each tool's results.
    let sources: Vec<&str> = xray
        .composition
        .by_source
        .iter()
        .map(|share| share.key.as_str())
        .collect();
    for expected in ["tool:read_file", "tool:grep", "tool:noop", "system", "user"] {
        assert!(
            sources.contains(&expected),
            "{expected} missing from {sources:?}"
        );
    }
    let total: f64 = xray
        .composition
        .by_kind
        .iter()
        .map(|share| share.share)
        .sum();
    assert!((total - 1.0).abs() < 1e-9, "{total}");
    assert_eq!(xray.totals.chunks, 10);
    assert_eq!(
        xray.totals.est_tokens,
        xray.chunks
            .iter()
            .map(|chunk| chunk.est_tokens)
            .sum::<u64>()
    );
    // The output is chunked too.
    assert_eq!(xray.response.len(), 1);
    assert_eq!(xray.response[0].preview, "Done.");
    assert!(!xray.provenance_partial && !xray.resolved_partial);

    // The previous call is step 4, and nothing it had was lost.
    let previous = xray.previous.clone().expect("previous");
    assert_eq!(previous.step_index, 4);
    assert_eq!(previous.shared_chunks, 6);
    assert!(xray.dropped_since_previous.is_empty());
    assert_eq!(xray.cache.cause, Cause::Stable);
}

#[test]
fn a_rewritten_result_is_attributed_by_its_tool_call_id() {
    let fixture = TempStore::open("xray-call-id", crate::Config::default());
    fixture
        .store()
        .ingest_batch(vec![
            llm(
                "s4",
                "l0",
                100,
                json!([msg("user", "list files")]),
                json!([call("c9", "ls", json!({}))]),
            ),
            tool(
                "s4",
                "t1",
                200,
                "ls",
                "c9",
                json!({}),
                "a.rs\nb.rs\nc.rs\nd.rs",
            ),
            llm(
                "s4",
                "l2",
                300,
                json!([
                    msg("user", "list files"),
                    call("c9", "ls", json!({})),
                    result("c9", "a.rs\nb.rs [2 more truncated]")
                ]),
                json!([msg("assistant", "ok")]),
            ),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-s4", "l2", options(20));
    let introduced = xray.chunks[2].introduced_by.clone().expect("attributed");
    assert_eq!(introduced.step_index, 1);
    assert_eq!(introduced.via, "tool_call_id");
    assert_eq!(introduced.name, "ls");
}

#[test]
fn compaction_loss_names_the_lost_instruction() {
    let fixture = TempStore::open("xray-compaction", crate::Config::default());
    let rule = "Never touch prod. Clean the logs.";
    fixture
        .store()
        .ingest_batch(vec![
            llm(
                "s2",
                "l0",
                100,
                json!([msg("system", SYSTEM), msg("user", rule)]),
                json!([msg("assistant", "ok")]),
            ),
            llm(
                "s2",
                "l1",
                200,
                json!([
                    msg("system", SYSTEM),
                    msg("user", rule),
                    msg("assistant", "ok"),
                    msg("user", "go on")
                ]),
                json!([msg("assistant", "done")]),
            ),
            llm(
                "s2",
                "l2",
                300,
                json!([msg("system", SYSTEM), msg("user", "Summary: logs cleaned.")]),
                json!([msg("assistant", "next")]),
            ),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-s2", "l2", options(20));
    assert_eq!(xray.previous.as_ref().map(|p| p.step_index), Some(1));
    let lost: Vec<(&str, Option<&str>)> = xray
        .dropped_since_previous
        .iter()
        .map(|dropped| (dropped.kind, dropped.preview.as_deref()))
        .collect();
    assert_eq!(
        lost,
        [
            ("instruction", Some(rule)),
            ("other", Some("ok")),
            ("instruction", Some("go on")),
        ]
    );
    assert_eq!(
        xray.dropped_since_previous[0]
            .introduced_by
            .as_ref()
            .map(|step| step.step_index),
        Some(0)
    );
    assert_eq!(xray.cache.cause, Cause::NewContext);
    let divergence = xray.cache.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.change, Change::Rewritten);
    assert_eq!(divergence.index, 1);
}

fn priced_config() -> crate::Config {
    let pricing = crate::pricing::Pricing::parse(
        r#"{"models": {"claude-x": {"input_per_mtok": 3.0, "output_per_mtok": 15.0,
                                    "cache_read_per_mtok": 0.3}}}"#,
    )
    .expect("pricing");
    crate::Config {
        pricing: std::sync::Arc::new(pricing),
        ..crate::Config::default()
    }
}

/// Four calls: a cold start, a timestamp edit in the system prompt, a stable
/// extension that the provider nonetheless did not serve from cache, and an
/// extension ten minutes later.
fn cache_run(model: &str) -> Vec<Span> {
    let system = |time: &str| format!("You are helpful. The time is 2026-10-17T{time}Z. Be brief.");
    let long = "context ".repeat(400);
    let calls = [
        (
            0,
            vec![msg("system", &system("09:14:03")), msg("user", &long)],
            Some(0),
        ),
        (
            SECOND,
            vec![
                msg("system", &system("09:19:44")),
                msg("user", &long),
                msg("assistant", "a1"),
            ],
            Some(0),
        ),
        (
            2 * SECOND,
            vec![
                msg("system", &system("09:19:44")),
                msg("user", &long),
                msg("assistant", "a1"),
                msg("user", "more"),
            ],
            Some(0),
        ),
        (
            2 * SECOND + 600 * SECOND,
            vec![
                msg("system", &system("09:19:44")),
                msg("user", &long),
                msg("assistant", "a1"),
                msg("user", "more"),
                msg("assistant", "a2"),
            ],
            None,
        ),
    ];
    calls
        .into_iter()
        .enumerate()
        .map(|(index, (start, input, read))| {
            let mut span = llm(
                "s3",
                &format!("l{index}"),
                1_000 + start,
                Value::Array(input),
                json!([]),
            );
            span.attributes
                .insert("gen_ai.request.model".into(), json!(model));
            span.attributes
                .insert("gen_ai.usage.input_tokens".into(), json!(900));
            span.attributes
                .insert("gen_ai.usage.output_tokens".into(), json!(10));
            if let Some(read) = read {
                span.attributes
                    .insert("gen_ai.usage.cache_read.input_tokens".into(), json!(read));
            }
            span
        })
        .collect()
}

#[test]
fn the_autopsy_names_causes_volatile_tokens_and_cost() {
    let fixture = TempStore::open("xray-autopsy", priced_config());
    fixture
        .store()
        .ingest_batch(cache_run("claude-x"))
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "s3", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let causes: Vec<Cause> = autopsy.steps.iter().map(|step| step.cause).collect();
    assert_eq!(
        causes,
        [
            Cause::NewContext,
            Cause::VolatileContent,
            Cause::Stable,
            Cause::TtlExpired
        ]
    );
    let edit = &autopsy.steps[1];
    assert_eq!(edit.previous_step_index, Some(0));
    assert_eq!(edit.gap_ns, Some(SECOND));
    let divergence = edit.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.index, 0);
    assert_eq!(divergence.role.as_deref(), Some("system"));
    assert_eq!(divergence.volatile, Some(Volatile::Timestamp));
    let diff = divergence.diff.expect("diff");
    assert_eq!(diff.before, "2026-10-17T09:14:03Z");
    assert_eq!(diff.after, "2026-10-17T09:19:44Z");
    assert!(
        edit.explanation.contains("(a timestamp)"),
        "{}",
        edit.explanation
    );
    // The long user chunk was carried and re-processed: tokens and money.
    assert!(edit.est_tokens_lost > 0);
    let cost = edit.est_cost_lost_usd.expect("priced");
    assert!(
        (cost - edit.est_tokens_lost as f64 * 2.7 / 1e6).abs() < 1e-9,
        "{cost}"
    );
    // Calibrated to the provider's count: anthropic reports 900 uncached
    // input tokens plus 0 read, so the raw estimate of the carried user
    // chunk (3,200 ASCII characters, 800 tokens) is scaled to that context.
    assert_eq!(
        edit.est_tokens_lost,
        (800.0 * 900.0 / edit.est_tokens as f64).round() as u64
    );
    assert_ne!(edit.est_tokens_lost, 800);

    let stable = &autopsy.steps[2];
    assert_eq!(stable.prefix_chunks_shared, 3);
    assert_eq!(stable.est_tokens_lost, 0);
    assert_eq!(stable.reported_hit, Some(false));
    let expired = &autopsy.steps[3];
    assert!(
        expired.explanation.contains("past the 5 min 0 s TTL"),
        "{}",
        expired.explanation
    );
    assert_eq!(expired.reported_hit, None);

    let summary = &autopsy.summary;
    assert_eq!(summary.steps, 4);
    assert_eq!(summary.breaks, 2);
    assert_eq!(summary.by_cause["stable"], 1);
    assert_eq!(summary.by_cause["reordered"], 0, "every cause is listed");
    assert_eq!(
        summary.disagreements, 1,
        "stable, yet no cache read reported"
    );
    assert_eq!(summary.unpriced_breaks, 0);
    assert!(summary
        .disagreement_advice
        .is_some_and(|advice| advice.contains("cache breakpoint")));
    let first = &summary.culprits[0];
    assert!(summary.culprits.len() == 2);
    let clock = summary
        .culprits
        .iter()
        .find(|culprit| culprit.volatile == Some(Volatile::Timestamp))
        .expect("the timestamp culprit");
    assert_eq!(clock.example_step_index, 1);
    assert!(clock.advice.contains("clock value"), "{}", clock.advice);
    let expiry = summary
        .culprits
        .iter()
        .find(|culprit| culprit.cause == Cause::TtlExpired)
        .expect("the expiry culprit");
    assert!(
        expiry.advice.contains("cache lifetime"),
        "{}",
        expiry.advice
    );
    assert!(first.est_tokens_lost >= summary.culprits[1].est_tokens_lost);
    let total: f64 = autopsy
        .steps
        .iter()
        .filter_map(|step| step.est_cost_lost_usd)
        .sum();
    assert!((summary.est_cost_lost_usd.expect("priced") - total).abs() < 1e-9);

    // A longer TTL keeps the last call warm.
    let lenient = fixture
        .store()
        .cache_autopsy(
            Some(""),
            "s3",
            AutopsyOptions {
                ttl_seconds: 3_600,
                ..AutopsyOptions::default()
            },
        )
        .expect("reads")
        .expect("found");
    assert_eq!(lenient.steps[3].cause, Cause::Stable);
}

#[test]
fn an_unpriced_model_reports_tokens_but_no_money() {
    let fixture = TempStore::open("xray-unpriced", priced_config());
    fixture
        .store()
        .ingest_batch(cache_run("private-model"))
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "s3", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    assert!(autopsy
        .steps
        .iter()
        .all(|step| step.est_cost_lost_usd.is_none()));
    assert!(autopsy.steps[1].est_tokens_lost > 0);
    assert_eq!(autopsy.summary.est_cost_lost_usd, None);
    assert_eq!(autopsy.summary.unpriced_breaks, 2);
}

#[test]
fn tenants_are_isolated_and_an_ambiguous_operator_read_is_refused() {
    let fixture = TempStore::open("xray-tenants", crate::Config::default());
    let mut acme = llm(
        "s9",
        "l0",
        100,
        json!([msg("user", "acme secret")]),
        json!([]),
    );
    acme.tenant = "acme".into();
    let mut beta = llm(
        "s9",
        "l0",
        100,
        json!([msg("user", "beta secret")]),
        json!([]),
    );
    beta.tenant = "beta".into();
    let mut only_acme = llm("s8", "l0", 100, json!([msg("user", "x")]), json!([]));
    only_acme.tenant = "acme".into();
    fixture
        .store()
        .ingest_batch(vec![acme, beta, only_acme])
        .expect("ingests");
    let store = fixture.store();
    assert_eq!(
        store.xray(None, "tr-s9", "l0", options(20)).expect("reads"),
        Err(XrayMiss::AmbiguousTenant(2))
    );
    let seen = xray_of(store, Some("beta"), "tr-s9", "l0", options(20));
    assert_eq!(seen.tenant, "beta");
    assert_eq!(seen.chunks[0].preview, "beta secret");
    // Unambiguous in the operator view, it reads under its own tenant.
    let operator = xray_of(store, None, "tr-s8", "l0", options(20));
    assert_eq!(operator.tenant, "acme");
    assert_eq!(
        store
            .xray(Some("beta"), "tr-s8", "l0", options(20))
            .expect("reads"),
        Err(XrayMiss::NoSpan)
    );
    assert!(store
        .cache_autopsy(Some("beta"), "s8", AutopsyOptions::default())
        .expect("reads")
        .is_none());
    let autopsy = store
        .cache_autopsy(Some("acme"), "s9", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    assert_eq!(autopsy.steps.len(), 1);
}

#[test]
fn offloaded_contexts_are_resolved_unless_asked_not_to() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-offload", config);
    let long = "a long instruction ".repeat(10);
    let mut call = llm("s5", "l0", 100, json!(null), json!([]));
    // Offload moves string attributes, which is how exporters carry messages.
    call.attributes.insert(
        "gen_ai.input.messages".into(),
        json!(json!([msg("system", &long), msg("user", "short")]).to_string()),
    );
    fixture.store().ingest_batch(vec![call]).expect("ingests");
    let stored = fixture.store().get_trace("tr-s5").expect("reads");
    assert!(
        stored[0].attributes["gen_ai.input.messages"]["$payload"].is_string(),
        "the messages were offloaded: {:?}",
        stored[0].attributes
    );
    let resolved = xray_of(fixture.store(), Some(""), "tr-s5", "l0", options(20));
    assert_eq!(resolved.chunks.len(), 2);
    assert_eq!(resolved.chunks[0].kind, ChunkKind::System);
    assert!(!resolved.resolved_partial);

    let opaque = xray_of(
        fixture.store(),
        Some(""),
        "tr-s5",
        "l0",
        XrayOptions {
            resolve: false,
            ..options(20)
        },
    );
    assert_eq!(opaque.chunks.len(), 1);
    assert_eq!(opaque.chunks[0].kind, ChunkKind::Other);
    assert!(opaque.resolved_partial);
    assert_eq!(opaque.cache.cause, Cause::Unknown);
    assert!(
        opaque.cache.explanation.contains("resolve=false"),
        "{}",
        opaque.cache.explanation
    );
}

#[test]
fn refusals_bounds_and_the_deadline() {
    let fixture = TempStore::open("xray-refusals", crate::Config::default());
    let many: Vec<Value> = (0..MAX_CONTEXT_CHUNKS + 5)
        .map(|index| msg("user", &format!("m{index}")))
        .collect();
    fixture
        .store()
        .ingest_batch(vec![
            llm("s6", "l0", 100, Value::Array(many), json!([])),
            tool("s6", "t1", 200, "x", "c1", json!({}), "r"),
        ])
        .expect("ingests");
    let store = fixture.store();
    let big = xray_of(store, Some(""), "tr-s6", "l0", options(20));
    assert_eq!(big.chunks.len(), MAX_CONTEXT_CHUNKS);
    assert!(big.chunks_truncated);
    assert_eq!(
        store
            .xray(Some(""), "tr-s6", "t1", options(20))
            .expect("reads"),
        Err(XrayMiss::NotAnLlmStep(StepKind::Tool))
    );
    assert_eq!(
        store
            .xray(Some(""), "tr-s6", "nope", options(20))
            .expect("reads"),
        Err(XrayMiss::NoSpan)
    );
    assert!(store
        .cache_autopsy(Some(""), "nope", AutopsyOptions::default())
        .expect("reads")
        .is_none());

    drop(fixture);
    let config = crate::Config {
        query_deadline: Some(std::time::Duration::ZERO),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-deadline", config);
    fixture.store().ingest_batch(agent_run()).expect("ingests");
    assert!(matches!(
        fixture.store().xray(Some(""), "tr-s1", "l6", options(20)),
        Err(crate::Error::DeadlineExceeded(_))
    ));
    assert!(matches!(
        fixture
            .store()
            .cache_autopsy(Some(""), "s1", AutopsyOptions::default()),
        Err(crate::Error::DeadlineExceeded(_))
    ));
}

#[test]
fn an_unrecorded_context_says_so_and_blocks_pairing() {
    let fixture = TempStore::open("xray-unrecorded", crate::Config::default());
    let mut blind = llm("s7", "l1", 200, json!(null), json!([]));
    blind.attributes.remove("gen_ai.input.messages");
    blind.attributes.remove("gen_ai.output.messages");
    fixture
        .store()
        .ingest_batch(vec![
            llm("s7", "l0", 100, json!([msg("user", "hello")]), json!([])),
            blind,
            llm(
                "s7",
                "l2",
                300,
                json!([msg("user", "hello"), msg("assistant", "hi")]),
                json!([]),
            ),
        ])
        .expect("ingests");
    let store = fixture.store();
    let unrecorded = xray_of(store, Some(""), "tr-s7", "l1", options(20));
    assert!(unrecorded.chunks.is_empty());
    assert_eq!(unrecorded.context_unavailable, Some("was not recorded"));
    assert_eq!(unrecorded.cache.cause, Cause::Unknown);
    // The next call cannot be honestly paired across the gap.
    let after = xray_of(store, Some(""), "tr-s7", "l2", options(20));
    assert_eq!(after.context_unavailable, None);
    assert_eq!(after.cache.cause, Cause::Unknown);
    assert!(
        after.cache.explanation.contains("step 1"),
        "{}",
        after.cache.explanation
    );
    assert!(after.previous.is_none());
}

#[test]
fn the_store_default_threshold_follows_the_config() {
    let fixture = TempStore::open(
        "xray-config",
        crate::Config {
            xray_stale_steps: 7,
            ..crate::Config::default()
        },
    );
    assert_eq!(fixture.store().xray_stale_steps(), 7);
    assert_eq!(
        crate::Config::default().xray_stale_steps,
        DEFAULT_STALE_STEPS
    );
}
