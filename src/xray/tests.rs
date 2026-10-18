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
    assert!(!xray.texts_partial && !xray.edited_truncated && !xray.duplicates_truncated);

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

#[test]
fn a_compaction_pairs_with_the_call_it_compacted_whatever_its_shape() {
    let fixture = TempStore::open("xray-compaction-shapes", crate::Config::default());
    let rule = "Never touch prod. Clean the logs.";
    let tools = json!([{"name": "shell", "description": "run a command"}]);
    let with_tools = |mut span: Span| {
        span.attributes
            .insert("gen_ai.tool.definitions".into(), json!(tools.to_string()));
        span
    };
    fixture
        .store()
        .ingest_batch(vec![
            // [S, rule] → [S, rule, ok, go on] → [S, summary, continue]: the
            // compacted call shares a third of its chunks with the call it
            // compacted, and half with the older, shorter one.
            llm(
                "c1",
                "l0",
                100,
                json!([msg("system", SYSTEM), msg("user", rule)]),
                json!([msg("assistant", "ok")]),
            ),
            llm(
                "c1",
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
                "c1",
                "l2",
                300,
                json!([
                    msg("system", SYSTEM),
                    msg("user", "Summary: logs cleaned."),
                    msg("user", "continue")
                ]),
                json!([msg("assistant", "next")]),
            ),
            // [tools, S, rule, …] → [tools, S, summary, …]: under half shared
            // either way; the lead past the tool definitions pairs them.
            with_tools(llm(
                "c2",
                "l0",
                100,
                json!([
                    msg("system", SYSTEM),
                    msg("user", rule),
                    msg("assistant", "a"),
                    msg("user", "b"),
                    msg("assistant", "c"),
                    msg("user", "d")
                ]),
                json!([msg("assistant", "e")]),
            )),
            with_tools(llm(
                "c2",
                "l1",
                200,
                json!([
                    msg("system", SYSTEM),
                    msg("user", "Summary: logs cleaned."),
                    msg("user", "continue"),
                    msg("user", "and report")
                ]),
                json!([msg("assistant", "f")]),
            )),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-c1", "l2", options(20));
    assert_eq!(xray.previous.as_ref().map(|p| p.step_index), Some(1));
    let lost: Vec<(&str, Option<&str>, bool)> = xray
        .dropped_since_previous
        .iter()
        .map(|dropped| (dropped.kind, dropped.preview.as_deref(), dropped.unverified))
        .collect();
    assert_eq!(
        lost,
        [
            ("instruction", Some(rule), false),
            ("other", Some("ok"), false),
            ("instruction", Some("go on"), false),
        ]
    );
    let divergence = xray.cache.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.change, Change::Rewritten);
    assert!(
        !xray.cache.explanation.contains("changed in place"),
        "the lost instruction is not a volatile token: {}",
        xray.cache.explanation
    );

    let tooled = xray_of(fixture.store(), Some(""), "tr-c2", "l1", options(20));
    assert_eq!(tooled.chunks[0].role.as_deref(), Some("tools"));
    assert_eq!(tooled.previous.as_ref().map(|p| p.step_index), Some(0));
    assert!(tooled
        .dropped_since_previous
        .iter()
        .any(|dropped| dropped.kind == "instruction" && dropped.preview.as_deref() == Some(rule)));
}

#[test]
fn agents_are_kept_apart_and_a_first_call_is_told_from_an_unpaired_one() {
    let fixture = TempStore::open("xray-agents", crate::Config::default());
    let named = |mut span: Span, agent: &str| {
        span.attributes
            .insert("gen_ai.agent.name".into(), json!(agent));
        span
    };
    fixture
        .store()
        .ingest_batch(vec![
            named(
                llm(
                    "a1",
                    "l0",
                    100,
                    json!([msg("system", SYSTEM), msg("user", "plan the release")]),
                    json!([msg("assistant", "delegating")]),
                ),
                "planner",
            ),
            // A worker on the same system prompt: unnamed, it would pair
            // with the planner and report the planner's task as lost.
            named(
                llm(
                    "a1",
                    "l1",
                    200,
                    json!([msg("system", SYSTEM), msg("user", "write the notes")]),
                    json!([msg("assistant", "written")]),
                ),
                "worker",
            ),
        ])
        .expect("ingests");
    let worker = xray_of(fixture.store(), Some(""), "tr-a1", "l1", options(20));
    assert!(worker.previous.is_none(), "{:?}", worker.previous);
    assert!(worker.dropped_since_previous.is_empty());
    assert_eq!(worker.cache.cause, Cause::NewContext);
    assert!(
        worker
            .cache
            .explanation
            .starts_with("no earlier call among the previous 32"),
        "{}",
        worker.cache.explanation
    );
    let planner = xray_of(fixture.store(), Some(""), "tr-a1", "l0", options(20));
    assert!(
        planner
            .cache
            .explanation
            .starts_with("the first model call of the run"),
        "{}",
        planner.cache.explanation
    );
}

#[test]
fn a_restamped_long_system_prompt_is_an_edit_not_a_lost_instruction() {
    let fixture = TempStore::open("xray-long-edit", crate::Config::default());
    // Over 80 KB, with the clock near the top.
    let system = |time: &str| {
        format!(
            "Now: 2026-10-17T{time}Z.\n{}",
            "Follow the policy. ".repeat(4_500)
        )
    };
    fixture
        .store()
        .ingest_batch(vec![
            llm(
                "e1",
                "l0",
                100,
                json!([msg("system", &system("09:14:03")), msg("user", "hi")]),
                json!([msg("assistant", "hello")]),
            ),
            llm(
                "e1",
                "l1",
                200,
                json!([
                    msg("system", &system("09:19:44")),
                    msg("user", "hi"),
                    msg("assistant", "hello")
                ]),
                json!([]),
            ),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-e1", "l1", options(20));
    assert!(xray.chunks[0].chars > 80_000);
    assert!(
        xray.dropped_since_previous.is_empty(),
        "{:?}",
        xray.dropped_since_previous
    );
    assert_eq!(xray.edited_since_previous.len(), 1);
    assert_eq!(xray.edited_since_previous[0].volatile, Volatile::Timestamp);
    assert_eq!(
        xray.edited_since_previous[0].diff.after,
        "2026-10-17T09:19:44Z"
    );
    let divergence = xray.cache.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.change, Change::Edited);
    // The policy text after the clock is what the cache lost.
    assert!(xray.cache.est_tokens_lost > 20_000, "{:?}", xray.cache);
}

#[test]
fn an_edit_that_could_not_be_checked_is_unverified_not_an_instruction_loss() {
    let fixture = TempStore::open("xray-unverified", crate::Config::default());
    let system = |time: &str| format!("You help. Now: 2026-10-17T{time}Z. Be brief and exact.");
    fixture
        .store()
        .ingest_batch(vec![
            llm(
                "u1",
                "l0",
                100,
                json!([
                    msg("system", &system("09:14:03")),
                    msg("user", "question one")
                ]),
                json!([msg("assistant", "a")]),
            ),
            llm(
                "u1",
                "l1",
                200,
                json!([
                    msg("system", &system("09:19:44")),
                    msg("user", "question one"),
                    msg("assistant", "a")
                ]),
                json!([]),
            ),
        ])
        .expect("ingests");
    let store = fixture.store();
    // Room for this call's own texts only: the previous system prompt's
    // text is refused, so the edit cannot be checked.
    let own: usize = system("09:19:44").len() + "question one".len() + "a".len();
    let target = store
        .xray_target(Some(""), "tr-u1", "l1", None)
        .expect("reads")
        .expect("found");
    let limits = Limits {
        pool_bytes: own,
        ..Limits::DEFAULT
    };
    let xray = store
        .xray_in(target, None, options(20), limits)
        .expect("reads")
        .expect("found");
    assert!(xray.texts_partial);
    assert_eq!(xray.dropped_since_previous.len(), 1);
    let dropped = &xray.dropped_since_previous[0];
    assert!(dropped.unverified, "{dropped:?}");
    assert_eq!(dropped.preview, None);
    // With room for both texts the same call shows the edit.
    let full = xray_of(store, Some(""), "tr-u1", "l1", options(20));
    assert!(full.dropped_since_previous.is_empty() && !full.texts_partial);
    assert_eq!(full.edited_since_previous.len(), 1);
}

#[test]
fn a_stateless_pipeline_is_not_a_cache_break() {
    let fixture = TempStore::open("xray-stateless", priced_config());
    let calls = [
        "Classify: the sky is green",
        "Classify: water is wet",
        "Classify: grass is blue",
    ];
    fixture
        .store()
        .ingest_batch(
            calls
                .iter()
                .enumerate()
                .map(|(index, input)| {
                    llm(
                        "p1",
                        &format!("l{index}"),
                        100 * (index as u64 + 1),
                        json!([msg("system", SYSTEM), msg("user", input)]),
                        json!([msg("assistant", "false")]),
                    )
                })
                .collect(),
        )
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "p1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let causes: Vec<Cause> = autopsy.steps.iter().map(|step| step.cause).collect();
    assert_eq!(causes, [Cause::NewContext, Cause::Stable, Cause::Stable]);
    let turn = &autopsy.steps[1];
    assert_eq!(turn.previous_step_index, Some(0));
    assert_eq!(turn.est_tokens_lost, 0);
    assert_eq!(
        turn.first_divergent_chunk.as_ref().map(|d| d.change),
        Some(Change::Replaced)
    );
    assert_eq!(autopsy.summary.breaks, 0);
    assert!(autopsy.summary.culprits.is_empty());
    assert_eq!(autopsy.summary.est_tokens_lost, 0);
}

#[test]
fn provenance_prefers_the_tool_that_returned_the_content_when_call_ids_repeat() {
    let fixture = TempStore::open("xray-repeated-ids", crate::Config::default());
    let ls = "alpha.rs beta.rs";
    let cat = "beta_contents_here and more";
    let base = vec![
        msg("user", "list then read"),
        call("c1", "ls", json!({})),
        result("c1", ls),
        // The framework numbers calls per turn: the next call reuses "c1".
        call("c1", "cat", json!({"path": "beta.rs"})),
    ];
    let run = |name: &str, shown: &str| {
        let mut last = base.clone();
        last.push(result("c1", shown));
        vec![
            llm(
                name,
                "l0",
                100,
                json!([msg("user", "list then read")]),
                json!([call("c1", "ls", json!({}))]),
            ),
            tool(name, "t1", 200, "ls", "c1", json!({}), ls),
            llm(
                name,
                "l2",
                300,
                Value::Array(base[..3].to_vec()),
                json!([call("c1", "cat", json!({"path": "beta.rs"}))]),
            ),
            tool(
                name,
                "t3",
                400,
                "cat",
                "c1",
                json!({"path": "beta.rs"}),
                cat,
            ),
            llm(
                name,
                "l4",
                500,
                Value::Array(last),
                json!([msg("assistant", "ok")]),
            ),
        ]
    };
    let mut spans = run("r1", cat);
    // The same run, the cat output truncated before the model saw it.
    spans.extend(run("r2", "beta_contents_here [truncated]"));
    fixture.store().ingest_batch(spans).expect("ingests");
    let exact = xray_of(fixture.store(), Some(""), "tr-r1", "l4", options(20));
    let from = exact.chunks[4].introduced_by.clone().expect("attributed");
    assert_eq!(
        (from.step_index, from.name.as_str(), from.via),
        (3, "cat", "content")
    );
    let rewritten = xray_of(fixture.store(), Some(""), "tr-r2", "l4", options(20));
    let from = rewritten.chunks[4]
        .introduced_by
        .clone()
        .expect("attributed");
    assert_eq!(
        (from.step_index, from.name.as_str(), from.via),
        (3, "cat", "tool_call_id"),
        "the latest tool step answering the id, not the first"
    );
    // The ls result keeps its own tool.
    let from = exact.chunks[2].introduced_by.clone().expect("attributed");
    assert_eq!((from.step_index, from.name.as_str()), (1, "ls"));
}

#[test]
fn rewritten_tool_arguments_break_the_cache_where_they_sit() {
    let fixture = TempStore::open("xray-reserialized", crate::Config::default());
    let openai = |id: &str, start: u64, input: Value| {
        let mut span = llm("o1", id, start, input, json!([]));
        span.attributes
            .insert("gen_ai.provider.name".into(), json!("openai"));
        span
    };
    let grep = |arguments: &str| {
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c1", "type": "function",
             "function": {"name": "grep", "arguments": arguments}}]})
    };
    fixture
        .store()
        .ingest_batch(vec![
            openai(
                "l0",
                100,
                json!([
                    msg("system", SYSTEM),
                    msg("user", TASK),
                    grep(r#"{"q":"parse","n":5}"#),
                    result("c1", GREP)
                ]),
            ),
            // The framework re-serialized the earlier call's arguments.
            openai(
                "l1",
                200,
                json!([
                    msg("system", SYSTEM),
                    msg("user", TASK),
                    grep(r#"{"n": 5, "q": "parse"}"#),
                    result("c1", GREP),
                    msg("assistant", "found it")
                ]),
            ),
        ])
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "o1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.cause, Cause::Reordered, "{}", step.explanation);
    let divergence = step.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.index, 2);
    assert_eq!(divergence.change, Change::Reserialized);
    let diff = divergence.diff.expect("diff");
    assert_ne!(diff.before, diff.after, "the bytes that changed are shown");
}

#[test]
fn calibration_is_skipped_when_the_estimate_cannot_cover_the_providers_count() {
    let fixture = TempStore::open("xray-calibration", priced_config());
    let long = "context ".repeat(400);
    let system = |time: &str| format!("You are helpful. The time is 2026-10-17T{time}Z. Be brief.");
    let user_with_image = json!({"role": "user", "content": [
        {"type": "text", "text": long},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                     "data": "AAAA".repeat(64)}}]});
    let calls = [
        (
            "l0",
            0,
            json!([msg("system", &system("09:14:03")), user_with_image.clone()]),
        ),
        (
            "l1",
            SECOND,
            json!([
                msg("system", &system("09:19:44")),
                user_with_image,
                msg("assistant", "a1")
            ]),
        ),
    ];
    fixture
        .store()
        .ingest_batch(
            calls
                .into_iter()
                .map(|(id, start, input)| {
                    let mut span = llm("m1", id, 1_000 + start, input, json!([]));
                    span.attributes
                        .insert("gen_ai.usage.input_tokens".into(), json!(5_000));
                    span.attributes
                        .insert("gen_ai.usage.output_tokens".into(), json!(10));
                    span
                })
                .collect(),
        )
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "m1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.cause, Cause::VolatileContent);
    // An image the estimate counts as zero: the provider's 5,000 would
    // inflate a ratio, so the raw estimate stands — the long text (800) and
    // the system prompt's tail after the clock (3).
    assert_eq!(step.calibration, None);
    assert_eq!(step.est_tokens_lost, 803);
}

#[test]
fn the_comparison_window_is_read_before_older_history() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-window-budget", config);
    let rule = "a long standing instruction ".repeat(20);
    let calls: Vec<Span> = (0..5)
        .map(|index| {
            let mut span = llm(
                "w1",
                &format!("l{index}"),
                100 * (index + 1),
                json!(null),
                json!([]),
            );
            span.attributes.insert(
                "gen_ai.input.messages".into(),
                json!(json!([
                    msg("system", &rule),
                    msg("user", &format!("question {index}"))
                ])
                .to_string()),
            );
            span
        })
        .collect();
    let payload = calls[4].attributes["gen_ai.input.messages"]
        .as_str()
        .expect("text")
        .len();
    fixture.store().ingest_batch(calls).expect("ingests");
    let store = fixture.store();
    // Room for the call itself and one more call of history.
    let limits = Limits {
        read_bytes: payload + payload / 2,
        ..Limits::DEFAULT
    };
    let target = store
        .xray_target(Some(""), "tr-w1", "l4", None)
        .expect("reads")
        .expect("found");
    let xray = store
        .xray_in(target, None, options(20), limits)
        .expect("reads")
        .expect("found");
    assert!(xray.resolved_partial, "older history was left unresolved");
    assert_eq!(
        xray.previous.as_ref().map(|previous| previous.step_index),
        Some(3),
        "the nearest call was read first: {}",
        xray.cache.explanation
    );

    // The autopsy gives each call its own budget out of the request's.
    let tape = store
        .run_tape(Some(""), "w1", MAX_TAPE_SPANS, MAX_TAPE_SPANS, None)
        .expect("reads");
    let per_call = Limits {
        read_bytes: payload + payload / 2,
        autopsy_read_bytes: 100 * payload,
        ..Limits::DEFAULT
    };
    let autopsy = store
        .autopsy_in(
            Some(""),
            "w1",
            &tape,
            None,
            AutopsyOptions::default(),
            per_call,
        )
        .expect("reads")
        .expect("found");
    assert!(!autopsy.resolved_partial);
    assert!(autopsy
        .steps
        .iter()
        .all(|step| step.cause != Cause::Unknown));
    let starved = Limits {
        autopsy_read_bytes: 2 * payload + payload / 2,
        ..per_call
    };
    let autopsy = store
        .autopsy_in(
            Some(""),
            "w1",
            &tape,
            None,
            AutopsyOptions::default(),
            starved,
        )
        .expect("reads")
        .expect("found");
    assert!(autopsy.resolved_partial);
    assert_eq!(autopsy.steps[3].cause, Cause::Unknown);
}

#[test]
fn culprits_are_ordered_by_cost_when_every_one_is_priced() {
    let pricing = crate::pricing::Pricing::parse(
        r#"{"models": {"claude-x": {"input_per_mtok": 3.0, "output_per_mtok": 15.0,
                                    "cache_read_per_mtok": 0.3},
                       "cheap-x": {"input_per_mtok": 0.1, "output_per_mtok": 0.4,
                                   "cache_read_per_mtok": 0.01}}}"#,
    )
    .expect("pricing");
    let fixture = TempStore::open(
        "xray-culprits",
        crate::Config {
            pricing: std::sync::Arc::new(pricing),
            ..crate::Config::default()
        },
    );
    let call = |id: &str, start: u64, model: &str, input: Value| {
        let mut span = llm("k1", id, start, input, json!([]));
        span.attributes
            .insert("gen_ai.request.model".into(), json!(model));
        span
    };
    let clock = |time: &str| format!("Cheap agent. Now 2026-10-17T{time}Z. Go.");
    let request = |id: &str| format!("Dear agent, request {id} follows. Go.");
    let long = "bulk ".repeat(800);
    let short = "note ".repeat(80);
    fixture
        .store()
        .ingest_batch(vec![
            // Many tokens lost, at a cheap rate: a timestamp.
            call(
                "a0",
                100,
                "cheap-x",
                json!([msg("system", &clock("09:00:00")), msg("user", &long)]),
            ),
            // Fewer tokens, at a dear rate: a request id.
            call(
                "b0",
                200,
                "claude-x",
                json!([
                    msg("system", &request("550e8400-e29b-41d4-a716-446655440000")),
                    msg("user", &short)
                ]),
            ),
            call(
                "a1",
                300,
                "cheap-x",
                json!([
                    msg("system", &clock("09:00:05")),
                    msg("user", &long),
                    msg("assistant", "x")
                ]),
            ),
            call(
                "b1",
                400,
                "claude-x",
                json!([
                    msg("system", &request("650e8400-e29b-41d4-a716-446655440000")),
                    msg("user", &short),
                    msg("assistant", "y")
                ]),
            ),
        ])
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "k1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let culprits = &autopsy.summary.culprits;
    assert_eq!(culprits.len(), 2, "{culprits:?}");
    assert!(culprits[1].est_tokens_lost > culprits[0].est_tokens_lost);
    assert_eq!(culprits[0].volatile, Some(Volatile::Uuid), "{culprits:?}");
    assert!(culprits[0].est_cost_lost_usd > culprits[1].est_cost_lost_usd);
    assert!(!autopsy.summary.culprits_truncated);
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
    // input tokens plus 0 read, so the raw estimate — the carried user chunk
    // (3,200 ASCII characters, 800 tokens) plus the system prompt's text
    // after the changed clock ("Z. Be brief.", 3 tokens), which the cache
    // could no longer serve either — is scaled to that context.
    let ratio = 900.0 / edit.est_tokens as f64;
    assert_eq!(edit.calibration, Some(ratio));
    assert_eq!(edit.est_tokens_lost, (803.0 * ratio).round() as u64);
    assert_ne!(edit.est_tokens_lost, 803);

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
fn a_cache_the_provider_served_did_not_expire_whatever_ttl_was_assumed() {
    // The last call of `cache_run` comes ten minutes after its predecessor,
    // past the assumed five-minute TTL, with its prefix intact. When the
    // provider reports reading that prefix from its cache, the cache lived
    // longer than assumed (Claude Code and Codex both run caches longer
    // than five minutes): nothing was lost, and nothing contradicts the
    // provider. A real coding-agent store showed this on 15 of 25 such
    // verdicts of one session, millions of tokens counted lost that the
    // provider had served.
    let served = |read: u64| {
        let mut spans = cache_run("claude-x");
        let last = spans.last_mut().expect("four calls");
        last.attributes
            .insert("gen_ai.usage.cache_read.input_tokens".into(), json!(read));
        let fixture = TempStore::open("xray-ttl-served", priced_config());
        fixture.store().ingest_batch(spans).expect("ingests");
        fixture
            .store()
            .cache_autopsy(Some(""), "s3", AutopsyOptions::default())
            .expect("reads")
            .expect("found")
    };
    // The predecessor reported a 900-token context; 850 of it read now.
    let autopsy = served(850);
    let last = &autopsy.steps[3];
    assert_eq!(last.cause, Cause::Stable, "{}", last.explanation);
    assert_eq!(last.est_tokens_lost, 0);
    assert_eq!(last.est_cost_lost_usd, Some(0.0));
    assert_eq!(last.gap_ns, Some(600 * SECOND));
    assert!(
        last.explanation.contains("past the 5 min 0 s TTL assumed")
            && last.explanation.contains("read 850"),
        "{}",
        last.explanation
    );
    assert_eq!(autopsy.summary.by_cause["ttl_expired"], 0);
    assert_eq!(autopsy.summary.breaks, 1, "only the timestamp edit");
    assert_eq!(
        autopsy.summary.disagreements, 1,
        "only the stable call with no read: the served one agrees"
    );
    // A read of a small part — what a longer-lived system prompt alone
    // would be — leaves the conversation's prefix expired.
    let autopsy = served(100);
    assert_eq!(autopsy.steps[3].cause, Cause::TtlExpired);
    assert!(autopsy.steps[3].est_tokens_lost > 0);
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
fn the_analysis_itself_honours_the_deadline_after_the_read() {
    // The run is read with no budget; the analysis is then handed one that
    // has already run out. A zero query deadline stops at the read, so this
    // is what shows the X-Ray's and the autopsy's own work is budgeted.
    let fixture = TempStore::open("xray-analysis-deadline", crate::Config::default());
    fixture.store().ingest_batch(agent_run()).expect("ingests");
    let store = fixture.store();
    let spent = || Some(crate::Deadline::capped(None, std::time::Duration::ZERO));
    let target = store
        .xray_target(Some(""), "tr-s1", "l6", None)
        .expect("reads")
        .expect("found");
    assert!(matches!(
        store.xray_in(target, spent(), options(20), Limits::DEFAULT),
        Err(crate::Error::DeadlineExceeded(_))
    ));
    let tape = store
        .run_tape(Some(""), "s1", MAX_TAPE_SPANS, MAX_TAPE_SPANS, None)
        .expect("reads");
    assert!(matches!(
        store.autopsy_in(
            Some(""),
            "s1",
            &tape,
            spent(),
            AutopsyOptions::default(),
            Limits::DEFAULT
        ),
        Err(crate::Error::DeadlineExceeded(_))
    ));
    // With time left, the same calls answer.
    let target = store
        .xray_target(Some(""), "tr-s1", "l6", None)
        .expect("reads")
        .expect("found");
    assert!(store
        .xray_in(target, None, options(20), Limits::DEFAULT)
        .expect("reads")
        .is_ok());
}

#[test]
fn a_step_of_a_run_is_xrayed_from_one_read() {
    let fixture = TempStore::open("xray-step", crate::Config::default());
    fixture.store().ingest_batch(agent_run()).expect("ingests");
    let store = fixture.store();
    let by_step = store
        .xray_step(Some(""), "s1", 6, options(2))
        .expect("reads")
        .expect("found");
    let by_span = xray_of(store, Some(""), "tr-s1", "l6", options(2));
    assert_eq!(by_step, by_span);
    assert_eq!(
        store
            .xray_step(Some(""), "s1", 9, options(2))
            .expect("reads"),
        Err(XrayMiss::NoStep {
            steps: 7,
            more: false
        })
    );
    assert_eq!(
        store
            .xray_step(Some(""), "s1", 1, options(2))
            .expect("reads"),
        Err(XrayMiss::NotAnLlmStep(StepKind::Tool))
    );
    assert_eq!(
        store
            .xray_step(Some(""), "nope", 0, options(2))
            .expect("reads"),
        Err(XrayMiss::NoRun)
    );
    // A run present in two tenants numbers its steps per tenant: the
    // operator view must not guess which one is meant.
    let mut other = agent_run();
    for span in &mut other {
        span.tenant = "beta".into();
    }
    store.ingest_batch(other).expect("ingests");
    assert_eq!(
        store.xray_step(None, "s1", 6, options(2)).expect("reads"),
        Err(XrayMiss::AmbiguousTenant(2))
    );
    let beta = store
        .xray_step(Some("beta"), "s1", 6, options(2))
        .expect("reads")
        .expect("found");
    assert_eq!(beta.tenant, "beta");
}

#[test]
fn a_call_xrayed_alone_reports_no_step_index_anywhere() {
    let fixture = TempStore::open("xray-fallback", crate::Config::default());
    fixture.store().ingest_batch(agent_run()).expect("ingests");
    let store = fixture.store();
    let mut target = store
        .xray_target(Some(""), "tr-s1", "l6", None)
        .expect("reads")
        .expect("found");
    // As if the call were not among its run's spans: its step is not in
    // the tape, so it is X-rayed alone.
    target.tape.steps.retain(|step| step.span_id != "l6");
    let xray = store
        .xray_in(target, None, options(20), Limits::DEFAULT)
        .expect("reads")
        .expect("found");
    assert_eq!(xray.step_index, None);
    assert_eq!(xray.cache.step_index, None);
    assert_eq!(xray.cache.cause, Cause::Unknown);
    assert!(xray.provenance_partial);
    assert_eq!(xray.chunks.len(), 10);
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

/// A model call that recorded only what it added to `conversation`
/// ([`INPUT_DELTA_ATTRIBUTE`]) — or, with `delta` false, its whole context —
/// as the recorders write transcripts.
fn delta_llm(
    id: &str,
    start: u64,
    conversation: &str,
    delta: bool,
    input: Value,
    output: Value,
) -> Span {
    let mut span = llm("d1", id, start, input, output);
    span.attributes
        .insert(CONVERSATION_ATTRIBUTE.into(), json!(conversation));
    span.attributes
        .insert(INPUT_DELTA_ATTRIBUTE.into(), json!(delta));
    span
}

/// A main conversation that reads a file, a helper conversation in between,
/// and the main one carrying on: every call recorded as a delta.
fn delta_run() -> Vec<Span> {
    vec![
        delta_llm(
            "m0",
            100,
            "main",
            true,
            json!([msg("system", SYSTEM), msg("user", TASK)]),
            json!([call("c1", "read_file", json!({"path": "parser.rs"}))]),
        ),
        tool(
            "d1",
            "t1",
            200,
            "read_file",
            "c1",
            json!({"path": "parser.rs"}),
            READ,
        ),
        delta_llm(
            "m2",
            300,
            "main",
            true,
            json!([result("c1", READ)]),
            json!([msg("assistant", "Asking a helper about zeta_marker_one")]),
        ),
        delta_llm(
            "h0",
            400,
            "helper",
            true,
            json!([msg("user", "Where is zeta_marker_one defined?")]),
            json!([msg("assistant", "In parser.rs")]),
        ),
        delta_llm(
            "m4",
            500,
            "main",
            true,
            json!([msg("user", "The helper says parser.rs")]),
            json!([msg("assistant", "Fixed.")]),
        ),
        // Nothing new since m4 (a retry): an empty delta, not an unrecorded
        // context.
        delta_llm(
            "m5",
            600,
            "main",
            true,
            Value::Null,
            json!([msg("assistant", "Fixed.")]),
        ),
    ]
}

#[test]
fn a_late_call_of_a_long_session_is_read_with_the_history_nearest_it() {
    // A coding-agent session longer than one read: two calls, twenty
    // thousand spans of other work, then a call that carries on the main
    // conversation, a compaction that starts it over, a call after it, and
    // a helper conversation that opens late. Every call is a delta. The
    // late calls used to be X-rayed alone — no step index, no context —
    // which made the X-Ray useless for the latest calls of exactly the
    // sessions worth X-raying.
    let fixture = TempStore::open("xray-long", crate::Config::default());
    let mut spans = vec![
        delta_llm(
            "m0",
            100,
            "main",
            true,
            json!([msg("system", SYSTEM), msg("user", TASK)]),
            json!([msg("assistant", "Reading")]),
        ),
        delta_llm(
            "m1",
            200,
            "main",
            true,
            json!([msg("user", "go on")]),
            json!([msg("assistant", "Going")]),
        ),
    ];
    let filler = MAX_TAPE_SPANS;
    for index in 0..filler {
        spans.push(span(
            "tr-d1",
            &format!("f{index:05}"),
            300 + index as u64 * 10,
            json!({"session.id": "d1"}),
        ));
    }
    let late = 300 + filler as u64 * 10;
    spans.extend([
        delta_llm(
            "m2",
            late,
            "main",
            true,
            json!([msg("user", "still going")]),
            json!([msg("assistant", "Compacting")]),
        ),
        delta_llm(
            "m3",
            late + 10,
            "main",
            false,
            json!([
                msg("system", SYSTEM),
                msg("user", "Summary: the parser bug")
            ]),
            json!([msg("assistant", "Resuming")]),
        ),
        delta_llm(
            "h0",
            late + 20,
            "helper",
            true,
            json!([msg("user", "Where is zeta_marker_one defined?")]),
            json!([msg("assistant", "In parser.rs")]),
        ),
        delta_llm(
            "m4",
            late + 30,
            "main",
            true,
            json!([msg("user", "after the compaction")]),
            json!([msg("assistant", "Fixed.")]),
        ),
    ]);
    fixture.store().ingest_batch(spans).expect("ingests");

    // After the compaction the context is rebuilt whole, its indices are
    // the run's, and its history before the read is said to be unread.
    let xray = xray_of(fixture.store(), Some(""), "tr-d1", "m4", options(20));
    assert_eq!(xray.context_unavailable, None, "{xray:?}");
    let chunks: Vec<&str> = xray
        .chunks
        .iter()
        .map(|chunk| chunk.preview.as_str())
        .collect();
    assert_eq!(
        chunks,
        [
            SYSTEM,
            "Summary: the parser bug",
            "Resuming",
            "after the compaction"
        ]
    );
    assert_eq!(xray.step_index, Some(5), "m0 m1 m2 m3 h0 before it");
    assert_eq!(xray.previous.as_ref().map(|p| p.step_index), Some(3));
    assert_eq!(xray.cache.step_index, Some(5));
    assert_eq!(xray.cache.previous_step_index, Some(3));
    assert_eq!(
        xray.chunks[3].introduced_by.as_ref().map(|i| i.step_index),
        Some(5)
    );
    assert!(xray.provenance_partial);
    // The same call by its step.
    let by_step = fixture
        .store()
        .xray_step(Some(""), "d1", 5, options(20))
        .expect("reads")
        .expect("found");
    assert_eq!(by_step.span, xray.span);
    assert_eq!(by_step.chunks.len(), 4);
    // A conversation that opened inside the read is read as it was.
    let helper = xray_of(fixture.store(), Some(""), "tr-d1", "h0", options(20));
    assert_eq!(helper.context_unavailable, None);
    assert_eq!(helper.step_index, Some(4));
    // Before the compaction, the main conversation's earlier calls lie
    // before the read: said so, never passed off as its whole context.
    let before = xray_of(fixture.store(), Some(""), "tr-d1", "m2", options(20));
    assert_eq!(before.context_unavailable, Some(BEGAN_BEFORE_READ));
    assert_eq!(before.step_index, Some(2));
    // A step past the run's end names how many there are.
    assert!(matches!(
        fixture.store().xray_step(Some(""), "d1", 9, options(20)),
        Ok(Err(XrayMiss::NoStep {
            steps: 6,
            more: false
        }))
    ));
}

#[test]
fn a_delta_recorded_conversation_is_read_as_the_whole_context_it_was() {
    let fixture = TempStore::open("xray-delta", crate::Config::default());
    let mut spans = delta_run();
    // An empty delta is recorded with no input at all, as the recorders do.
    spans[5].attributes.remove("gen_ai.input.messages");
    fixture.store().ingest_batch(spans).expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-d1", "m4", options(20));
    let chunks: Vec<(&str, &str)> = xray
        .chunks
        .iter()
        .map(|chunk| (chunk.role.as_deref().unwrap_or("-"), chunk.preview.as_str()))
        .collect();
    // The main conversation's calls, their answers and what each added, in
    // order — and nothing of the helper's, which ran in between.
    assert_eq!(
        chunks,
        [
            ("system", SYSTEM),
            ("user", TASK),
            ("assistant", "read_file({\"path\":\"parser.rs\"})"),
            ("tool", READ),
            ("assistant", "Asking a helper about zeta_marker_one"),
            ("user", "The helper says parser.rs"),
        ]
    );
    assert_eq!(xray.context_unavailable, None);
    // Provenance reaches back through the conversation: the result came from
    // the tool step, the task from the first call.
    let from = |index: usize| {
        xray.chunks[index]
            .introduced_by
            .clone()
            .expect("introduced")
    };
    assert_eq!((from(1).step_index, from(3).step_index), (0, 1));
    assert_eq!(from(3).kind, StepKind::Tool);
    // Its predecessor is the main conversation's previous call, not the
    // helper's nearer one, and nothing was lost.
    let previous = xray.previous.clone().expect("previous");
    assert_eq!(previous.step_index, 2);
    assert_eq!(previous.shared_chunks, 4, "the whole of step 2's context");
    assert!(xray.dropped_since_previous.is_empty());
    assert_eq!(xray.cache.cause, Cause::Stable);

    // An empty delta is the conversation so far plus the last answer.
    let retry = xray_of(fixture.store(), Some(""), "tr-d1", "m5", options(20));
    assert_eq!(retry.context_unavailable, None);
    assert_eq!(retry.chunks.len(), 7);
    assert_eq!(retry.chunks[6].preview, "Fixed.");

    // The autopsy pairs each call within its conversation the same way.
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "d1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let verdicts: Vec<(Option<usize>, Option<usize>, Cause)> = autopsy
        .steps
        .iter()
        .map(|step| (step.step_index, step.previous_step_index, step.cause))
        .collect();
    assert_eq!(
        verdicts,
        [
            (Some(0), None, Cause::NewContext),
            (Some(2), Some(0), Cause::Stable),
            (Some(3), None, Cause::NewContext),
            (Some(4), Some(2), Cause::Stable),
            (Some(5), Some(4), Cause::Stable),
        ]
    );
    assert!(
        autopsy.steps[2].explanation.contains("conversation"),
        "{}",
        autopsy.steps[2].explanation
    );
}

#[test]
fn a_restarted_conversation_pairs_with_the_call_it_compacted_and_names_the_loss() {
    let fixture = TempStore::open("xray-delta-restart", crate::Config::default());
    let rule = "Never touch the migrations folder";
    fixture
        .store()
        .ingest_batch(vec![
            delta_llm(
                "c0",
                100,
                "main",
                true,
                json!([msg("system", SYSTEM), msg("user", rule)]),
                json!([msg("assistant", "Understood.")]),
            ),
            delta_llm(
                "c1",
                200,
                "main",
                true,
                json!([msg("user", "Fix the login test")]),
                json!([msg("assistant", "Done.")]),
            ),
            // After a compaction the recorders mark the call `false`: its
            // input is all it saw — and shares nothing with what came before.
            delta_llm(
                "c2",
                300,
                "main",
                false,
                json!([msg("user", "Summary: the login test was fixed.")]),
                json!([msg("assistant", "Next?")]),
            ),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-d1", "c2", options(20));
    assert_eq!(xray.chunks.len(), 1);
    assert_eq!(
        xray.previous
            .as_ref()
            .map(|p| (p.step_index, p.shared_chunks)),
        Some((1, 0)),
        "linked by its conversation, though nothing is shared"
    );
    let lost: Vec<(&str, &str)> = xray
        .dropped_since_previous
        .iter()
        .map(|dropped| (dropped.kind, dropped.preview.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        lost,
        [
            ("instruction", SYSTEM),
            ("instruction", rule),
            ("other", "Understood."),
            ("instruction", "Fix the login test"),
        ]
    );
    assert_eq!(xray.cache.cause, Cause::NewContext);
    let divergence = xray.cache.first_divergent_chunk.clone().expect("diverged");
    assert_eq!(divergence.change, Change::Rewritten);
}

#[test]
fn threads_are_bounded_and_a_dropped_conversation_is_not_rebuilt_from_nothing() {
    let read_of = |text: &str| Read {
        context: chunk::context_pieces(Some(&json!([msg("user", text)])), &[], MAX_CONTEXT_CHUNKS),
        response: chunk::response_pieces(&json!([msg("assistant", "ok")]), MAX_CONTEXT_CHUNKS).list,
        unavailable: None,
        definitions_unavailable: None,
        response_unresolved: false,
        unresolved: false,
    };
    let span_of =
        |conversation: &str| delta_llm("x", 1, conversation, true, Value::Null, Value::Null);
    let mut threads = Threads::default();
    // One more conversation than are kept: the first, least recently
    // continued, is dropped.
    for index in 0..=MAX_THREADS {
        let read = threads.fold(&span_of(&format!("c{index}")), read_of("hello"));
        assert_eq!(read.context.list.len(), 1);
    }
    assert_eq!(threads.running.len(), MAX_THREADS);
    // Its next delta cannot be rebuilt, and says so rather than passing for
    // its whole context.
    let read = threads.fold(&span_of("c0"), read_of("again"));
    assert_eq!(read.unavailable, Some(NOT_REBUILT));
    // A kept one continues: its context, its answer, then the new message.
    let read = threads.fold(&span_of(&format!("c{MAX_THREADS}")), read_of("again"));
    assert_eq!(read.unavailable, None);
    let texts: Vec<&str> = read.context.list.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, ["hello", "ok", "again"]);
    // A call naming no conversation passes through untouched.
    let mut plain = span_of("c1");
    plain.attributes.remove(CONVERSATION_ATTRIBUTE);
    let read = threads.fold(&plain, read_of("solo"));
    assert_eq!(read.context.list.len(), 1);
    // An earlier call that could not be read leaves its successors partial,
    // said as such rather than blamed on their own requests.
    let mut unreadable = read_of("offloaded");
    unreadable.unavailable = Some("was offloaded and could not be resolved");
    threads.fold(&span_of("dark"), unreadable);
    let read = threads.fold(&span_of("dark"), read_of("after"));
    assert_eq!(read.unavailable, Some(EARLIER_UNREADABLE));
    // An empty delta — a call that recorded no input — continues a readable
    // conversation as a whole context, not an unrecorded one.
    let mut empty = read_of("unused");
    empty.context.list.clear();
    empty.unavailable = Some(NOT_RECORDED);
    let read = threads.fold(&span_of(&format!("c{MAX_THREADS}")), empty);
    assert_eq!(read.unavailable, None);
    assert_eq!(read.context.list.len(), 4, "hello, ok, again, ok");
    // The text bound holds too: one conversation larger than every bound
    // is never kept, so its next call is not rebuilt.
    let mut huge = Threads::default();
    let big = "x".repeat(MAX_THREAD_BYTES + 1);
    huge.fold(&span_of("big"), read_of(&big));
    assert!(huge.running.is_empty() && huge.bytes == 0);
    let read = huge.fold(&span_of("big"), read_of("next"));
    assert_eq!(read.unavailable, Some(NOT_REBUILT));
}

/// `count` distinct user turns.
fn turns(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| msg("user", &format!("turn {index}")))
        .collect()
}

#[test]
fn a_divergence_past_the_chunk_bound_is_not_called_stable() {
    let fixture = TempStore::open("xray-cut-stable", crate::Config::default());
    // Two calls identical in their first 2,000 chunks, differing after.
    let before = turns(MAX_CONTEXT_CHUNKS + 5);
    let mut after = before.clone();
    after[MAX_CONTEXT_CHUNKS + 2] = msg("user", "a different turn past the bound");
    after.push(msg("user", "the next question"));
    fixture
        .store()
        .ingest_batch(vec![
            llm("cut1", "l0", 100, Value::Array(before), json!([])),
            llm("cut1", "l1", 200, Value::Array(after), json!([])),
        ])
        .expect("ingests");
    let store = fixture.store();
    let autopsy = store
        .cache_autopsy(Some(""), "cut1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.previous_step_index, Some(0));
    assert_ne!(step.cause, Cause::Stable, "{}", step.explanation);
    assert_eq!(step.cause, Cause::Unknown, "{}", step.explanation);
    assert_eq!(step.prefix_chunks_shared, MAX_CONTEXT_CHUNKS);
    assert_eq!(step.est_tokens_lost, 0);
    assert!(step.chunks_truncated);
    assert!(autopsy.chunks_truncated, "the autopsy says it was cut");
    assert!(!autopsy.truncated, "every call was read");
    assert!(
        step.explanation
            .contains("both this context and step 0's had more than 2,000 chunks"),
        "{}",
        step.explanation
    );
    let xray = xray_of(store, Some(""), "tr-cut1", "l1", options(20));
    assert!(xray.chunks_truncated);
    assert!(xray.cache.chunks_truncated);
    assert_eq!(
        xray.cache.cause,
        Cause::Unknown,
        "{}",
        xray.cache.explanation
    );

    // A predecessor read whole that a cut context starts with is still
    // stable: nothing past the cut can change that.
    drop(fixture);
    let fixture = TempStore::open("xray-cut-prefix", crate::Config::default());
    let before = turns(MAX_CONTEXT_CHUNKS - 10);
    let after = turns(MAX_CONTEXT_CHUNKS + 10);
    fixture
        .store()
        .ingest_batch(vec![
            llm("cut3", "l0", 100, Value::Array(before), json!([])),
            llm("cut3", "l1", 200, Value::Array(after), json!([])),
        ])
        .expect("ingests");
    let autopsy = fixture
        .store()
        .cache_autopsy(Some(""), "cut3", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.cause, Cause::Stable, "{}", step.explanation);
    assert!(step.chunks_truncated && autopsy.chunks_truncated);
    assert!(!autopsy.steps[0].chunks_truncated);
}

#[test]
fn absence_past_the_chunk_bound_is_not_a_verified_loss() {
    let fixture = TempStore::open("xray-cut-loss", crate::Config::default());
    // Exactly as many chunks as are analysed: read whole.
    let before = turns(MAX_CONTEXT_CHUNKS);
    // One instruction put ahead of all of it: the last turn falls past the
    // bound, though nothing was removed.
    let mut after = vec![msg(
        "user",
        "A new instruction placed ahead of the whole history, far longer than any turn",
    )];
    after.extend(before.iter().cloned());
    fixture
        .store()
        .ingest_batch(vec![
            llm("cut2", "l0", 100, Value::Array(before), json!([])),
            llm("cut2", "l1", 200, Value::Array(after), json!([])),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-cut2", "l1", options(20));
    assert!(xray.chunks_truncated);
    assert_eq!(xray.previous.as_ref().map(|p| p.step_index), Some(0));
    assert!(
        xray.dropped_since_previous
            .iter()
            .all(|dropped| dropped.unverified),
        "{:?}",
        xray.dropped_since_previous
    );
    let lost: Vec<Option<&str>> = xray
        .dropped_since_previous
        .iter()
        .map(|dropped| dropped.preview.as_deref())
        .collect();
    assert_eq!(lost, [Some("turn 1999")], "listed, as not proven lost");
    // What was found is still a fact: the insertion ahead of the history,
    // which rests on finds here and absences from a predecessor read whole.
    assert_eq!(xray.cache.cause, Cause::VolatileContent);
    assert_eq!(
        xray.cache.first_divergent_chunk.as_ref().map(|d| d.change),
        Some(Change::Inserted)
    );

    // A chunk found edited past nothing but a cut is still an edit, not an
    // unverified loss.
    drop(fixture);
    let fixture = TempStore::open("xray-cut-edit", crate::Config::default());
    let rule = |version: &str| format!("Always answer in English. Policy version {version}.");
    let mut before = vec![msg("system", &rule("1.0.3"))];
    before.extend(turns(10));
    let mut after = vec![msg("system", &rule("1.0.4"))];
    after.extend(turns(MAX_CONTEXT_CHUNKS + 1));
    fixture
        .store()
        .ingest_batch(vec![
            llm("cut4", "l0", 100, Value::Array(before), json!([])),
            llm("cut4", "l1", 200, Value::Array(after), json!([])),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-cut4", "l1", options(20));
    assert!(xray.chunks_truncated);
    assert!(xray.dropped_since_previous.is_empty());
    assert_eq!(xray.edited_since_previous.len(), 1);
    assert_eq!(xray.edited_since_previous[0].diff.after, "1.0.4");
}

#[test]
fn absence_from_an_unread_request_is_not_a_verified_loss() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-unread-loss", config);
    // The earlier call's messages are inline; the later call's are offloaded
    // and, with resolve=false, not read. Both name one conversation, so they
    // pair whatever they share.
    let earlier = delta_llm(
        "u0",
        100,
        "main",
        false,
        json!([
            msg("system", "Be brief."),
            msg("user", "Never touch migrations")
        ]),
        json!([]),
    );
    let mut later = delta_llm("u1", 200, "main", false, json!(null), json!([]));
    later.attributes.insert(
        "gen_ai.input.messages".into(),
        json!(json!([
            msg("system", "Be brief."),
            msg("user", "Never touch migrations"),
            msg("user", &"a long question ".repeat(10))
        ])
        .to_string()),
    );
    fixture
        .store()
        .ingest_batch(vec![earlier, later])
        .expect("ingests");
    let xray = xray_of(
        fixture.store(),
        Some(""),
        "tr-d1",
        "u1",
        XrayOptions {
            resolve: false,
            ..options(20)
        },
    );
    assert_eq!(
        xray.context_unavailable,
        Some("was offloaded and resolve=false")
    );
    assert_eq!(xray.previous.as_ref().map(|p| p.step_index), Some(0));
    assert_eq!(xray.dropped_since_previous.len(), 2);
    assert!(
        xray.dropped_since_previous
            .iter()
            .all(|dropped| dropped.unverified),
        "{:?}",
        xray.dropped_since_previous
    );
    assert_eq!(xray.cache.cause, Cause::Unknown);
    // Read, the same call lost nothing.
    let read = xray_of(fixture.store(), Some(""), "tr-d1", "u1", options(20));
    assert_eq!(read.context_unavailable, None);
    assert!(read.dropped_since_previous.is_empty());

    // Partly read: the system instructions are inline and resolve, the
    // offloaded messages do not fit the budget. The system prompt is found;
    // the turns are not, and are not called lost.
    drop(fixture);
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-partly-read-loss", config);
    let earlier = delta_llm(
        "p0",
        100,
        "main",
        false,
        json!([
            msg("system", "Be brief."),
            msg("user", "Never touch migrations")
        ]),
        json!([]),
    );
    let mut later = delta_llm("p1", 200, "main", false, json!(null), json!([]));
    later.attributes.insert(
        "gen_ai.system_instructions".into(),
        json!([{"type": "text", "content": "Be brief."}]),
    );
    later.attributes.insert(
        "gen_ai.input.messages".into(),
        json!(json!([
            msg("user", "Never touch migrations"),
            msg("user", &"a long question ".repeat(10))
        ])
        .to_string()),
    );
    fixture
        .store()
        .ingest_batch(vec![earlier, later])
        .expect("ingests");
    let store = fixture.store();
    let target = store
        .xray_target(Some(""), "tr-d1", "p1", None)
        .expect("reads")
        .expect("found");
    let starved = Limits {
        read_bytes: 1,
        ..Limits::DEFAULT
    };
    let xray = store
        .xray_in(target, None, options(20), starved)
        .expect("reads")
        .expect("found");
    assert_eq!(
        xray.context_unavailable,
        Some("was offloaded and could not be resolved")
    );
    assert_eq!(
        xray.chunks[0].preview, "Be brief.",
        "the part that was read"
    );
    let lost: Vec<(&str, bool)> = xray
        .dropped_since_previous
        .iter()
        .map(|dropped| (dropped.preview.as_deref().unwrap_or(""), dropped.unverified))
        .collect();
    assert_eq!(lost, [("Never touch migrations", true)]);
}

#[test]
fn conversations_are_told_apart_by_their_whole_id() {
    let fixture = TempStore::open("xray-long-ids", crate::Config::default());
    // Two ids that share their first 300 characters.
    let shared = "c".repeat(300);
    let (first, second) = (format!("{shared}-first"), format!("{shared}-second"));
    fixture
        .store()
        .ingest_batch(vec![
            delta_llm(
                "a0",
                100,
                &first,
                true,
                json!([msg("user", "Task for the first")]),
                json!([msg("assistant", "The first's answer")]),
            ),
            delta_llm(
                "b0",
                200,
                &second,
                true,
                json!([msg("user", "Task for the second")]),
                json!([msg("assistant", "The second's answer")]),
            ),
        ])
        .expect("ingests");
    let xray = xray_of(fixture.store(), Some(""), "tr-d1", "b0", options(20));
    let texts: Vec<&str> = xray
        .chunks
        .iter()
        .map(|chunk| chunk.preview.as_str())
        .collect();
    assert_eq!(texts, ["Task for the second"]);
    assert!(xray.previous.is_none(), "{:?}", xray.previous);
}

#[test]
fn a_delta_call_whose_conversation_cannot_be_read_is_partial() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-unread-id", config);
    // An id long enough to be offloaded: it cannot name the conversation.
    let id = "conversation-".repeat(8);
    let mut nameless = delta_llm(
        "n1",
        300,
        "unused",
        true,
        json!([msg("user", "And then?")]),
        json!([]),
    );
    nameless.attributes.remove(CONVERSATION_ATTRIBUTE);
    fixture
        .store()
        .ingest_batch(vec![
            delta_llm(
                "o0",
                100,
                &id,
                true,
                json!([msg("user", "Fix the parser")]),
                json!([msg("assistant", "Fixed.")]),
            ),
            delta_llm(
                "o1",
                200,
                &id,
                true,
                json!([msg("user", "Now the lexer")]),
                json!([]),
            ),
            nameless,
        ])
        .expect("ingests");
    let store = fixture.store();
    let stored = store.get_trace("tr-d1").expect("reads");
    assert!(stored[0].attributes[CONVERSATION_ATTRIBUTE]["$payload"].is_string());
    // Only what each call added was recorded, and the conversation it adds
    // to cannot be found: its context is partial, not whole.
    for span in ["o1", "n1"] {
        let xray = xray_of(store, Some(""), "tr-d1", span, options(20));
        assert!(xray.context_unavailable.is_some(), "{span}");
        assert_eq!(xray.cache.cause, Cause::Unknown, "{span}");
    }
}

#[test]
fn an_unresolved_answer_leaves_the_delta_after_it_partial() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-unresolved-answer", config);
    let mut first = delta_llm(
        "m0",
        100,
        "main",
        true,
        json!([msg("user", "Fix the parser")]),
        json!(null),
    );
    // The answer is offloaded.
    first.attributes.insert(
        "gen_ai.output.messages".into(),
        json!(json!([msg("assistant", &"a long answer ".repeat(10))]).to_string()),
    );
    let mut second = delta_llm(
        "m1",
        200,
        "main",
        true,
        json!([msg("user", "Now the lexer")]),
        json!([]),
    );
    second
        .attributes
        .insert("gen_ai.usage.input_tokens".into(), json!(50));
    fixture
        .store()
        .ingest_batch(vec![first, second])
        .expect("ingests");
    let store = fixture.store();
    // No room for any payload: the answer stays a reference.
    let starved = Limits {
        read_bytes: 1,
        ..Limits::DEFAULT
    };
    let tape = store
        .run_tape(Some(""), "d1", MAX_TAPE_SPANS, MAX_TAPE_SPANS, None)
        .expect("reads");
    let autopsy = store
        .autopsy_in(
            Some(""),
            "d1",
            &tape,
            None,
            AutopsyOptions::default(),
            starved,
        )
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.cause, Cause::Unknown, "{}", step.explanation);
    assert_eq!(step.calibration, None);
    let target = store
        .xray_target(Some(""), "tr-d1", "m1", None)
        .expect("reads")
        .expect("found");
    let xray = store
        .xray_in(target, None, options(20), starved)
        .expect("reads")
        .expect("found");
    assert_eq!(
        xray.context_unavailable,
        Some(EARLIER_ANSWER_UNREADABLE),
        "{:?}",
        xray.chunks
    );
    assert_eq!(xray.cache.cause, Cause::Unknown);
    assert_eq!(xray.cache.calibration, None);
    // The call after it inherits the gap: its context carries the same
    // placeholder.
    let mut threads = Threads::default();
    let read = |unresolved: bool| Read {
        context: chunk::context_pieces(Some(&json!([msg("user", "q")])), &[], MAX_CONTEXT_CHUNKS),
        response: Vec::new(),
        unavailable: None,
        definitions_unavailable: None,
        response_unresolved: unresolved,
        unresolved,
    };
    let span = delta_llm("x", 1, "main", true, Value::Null, Value::Null);
    threads.fold(&span, read(true));
    let second = threads.fold(&span, read(false));
    assert_eq!(second.unavailable, Some(EARLIER_ANSWER_UNREADABLE));
    let third = threads.fold(&span, read(false));
    assert_eq!(third.unavailable, Some(EARLIER_UNREADABLE));

    // With room to resolve the answer, the same call is read whole.
    let autopsy = store
        .cache_autopsy(Some(""), "d1", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    assert_eq!(autopsy.steps[1].cause, Cause::Stable);
    let xray = xray_of(store, Some(""), "tr-d1", "m1", options(20));
    assert_eq!(xray.context_unavailable, None);
    assert_eq!(xray.chunks.len(), 3);
}

#[test]
fn unresolved_tool_definitions_neither_calibrate_nor_compare() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-unresolved-definitions", config);
    let definitions = json!([{"name": "read_file", "description": "Reads a file from disk",
                              "parameters": {"type": "object"}}])
    .to_string();
    let history = [
        msg("user", "Read the parser"),
        call("c1", "read_file", json!({"path": "parser.rs"})),
        result("c1", READ),
    ];
    let calls: Vec<Span> = [("l0", 100, 2), ("l1", 200, 3)]
        .into_iter()
        .map(|(id, start, upto)| {
            let input = history[..upto].to_vec();
            let mut span = llm("defs", id, start, Value::Array(input), json!([]));
            span.attributes
                .insert("gen_ai.tool.definitions".into(), json!(definitions));
            span.attributes
                .insert("gen_ai.usage.input_tokens".into(), json!(400));
            span
        })
        .collect();
    fixture.store().ingest_batch(calls).expect("ingests");
    let store = fixture.store();
    let stored = store.get_trace("tr-defs").expect("reads");
    assert!(
        stored[0].attributes["gen_ai.tool.definitions"]["$payload"].is_string(),
        "the definitions were offloaded"
    );
    let starved = Limits {
        read_bytes: 1,
        ..Limits::DEFAULT
    };
    let tape = store
        .run_tape(Some(""), "defs", MAX_TAPE_SPANS, MAX_TAPE_SPANS, None)
        .expect("reads");
    let autopsy = store
        .autopsy_in(
            Some(""),
            "defs",
            &tape,
            None,
            AutopsyOptions::default(),
            starved,
        )
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.calibration, None, "{}", step.explanation);
    assert_eq!(step.cause, Cause::Unknown, "{}", step.explanation);
    assert!(
        step.explanation
            .contains("carried tool definitions that were offloaded"),
        "{}",
        step.explanation
    );
    let target = store
        .xray_target(Some(""), "tr-defs", "l1", None)
        .expect("reads")
        .expect("found");
    let xray = store
        .xray_in(target, None, options(20), starved)
        .expect("reads")
        .expect("found");
    assert_eq!(
        xray.context_unavailable,
        Some(definitions_unread(true)),
        "{:?}",
        xray.chunks
    );

    // Resolved, the definitions are read and the estimate calibrates.
    let autopsy = store
        .cache_autopsy(Some(""), "defs", AutopsyOptions::default())
        .expect("reads")
        .expect("found");
    let step = &autopsy.steps[1];
    assert_eq!(step.cause, Cause::Stable, "{}", step.explanation);
    assert!(step.calibration.is_some());
}

#[test]
fn an_unread_answer_is_not_taken_for_silence() {
    let config = crate::Config {
        payload_threshold: Some(64),
        ..crate::Config::default()
    };
    let fixture = TempStore::open("xray-unread-said", config);
    let read_call = call("c1", "read_file", json!({"path": "parser.rs"}));
    let mut quoting = llm(
        "said",
        "l2",
        300,
        json!([msg("user", TASK), read_call.clone(), result("c1", READ)]),
        json!(null),
    );
    // The answer that draws on the result is offloaded, and its quote lies
    // past the reference's preview.
    let answer = format!("{} zeta_marker_one is the culprit", "Looking. ".repeat(40));
    quoting.attributes.insert(
        "gen_ai.output.messages".into(),
        json!(json!([msg("assistant", &answer)]).to_string()),
    );
    fixture
        .store()
        .ingest_batch(vec![
            llm(
                "said",
                "l0",
                100,
                json!([msg("user", TASK)]),
                json!([read_call.clone()]),
            ),
            tool(
                "said",
                "t1",
                200,
                "read_file",
                "c1",
                json!({"path": "parser.rs"}),
                READ,
            ),
            quoting,
            llm(
                "said",
                "l3",
                400,
                json!([
                    msg("user", TASK),
                    read_call,
                    result("c1", READ),
                    msg("user", "and now?")
                ]),
                json!([msg("assistant", "Done.")]),
            ),
        ])
        .expect("ingests");
    let store = fixture.store();
    let starved = Limits {
        read_bytes: 1,
        ..Limits::DEFAULT
    };
    let target = store
        .xray_target(Some(""), "tr-said", "l3", None)
        .expect("reads")
        .expect("found");
    let xray = store
        .xray_in(target, None, options(1), starved)
        .expect("reads")
        .expect("found");
    let result = xray
        .chunks
        .iter()
        .find(|chunk| chunk.kind == ChunkKind::ToolResult)
        .expect("the result");
    assert_eq!(result.age_steps, Some(2));
    assert!(!result.stale, "an answer nobody could read may quote it");
    assert!(xray.provenance_partial);
    // Read, the answer quotes it: not stale either way, and complete.
    let xray = xray_of(store, Some(""), "tr-said", "l3", options(1));
    assert!(!xray.provenance_partial);
    assert!(xray.chunks.iter().all(|chunk| !chunk.stale));
}
