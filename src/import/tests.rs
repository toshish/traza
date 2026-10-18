//! Import unit tests, over the synthetic fixtures in
//! `tests/fixtures/import/` (authored for these tests; no real transcript
//! content).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::Span;

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("import")
}

fn cc_main() -> PathBuf {
    fixtures()
        .join("claude-code")
        .join("-tmp-demo-shop")
        .join(format!("{S1}.jsonl"))
}

fn cc_agent() -> PathBuf {
    fixtures()
        .join("claude-code")
        .join("-tmp-demo-shop")
        .join(S1)
        .join("subagents")
        .join("agent-a1b2c3.jsonl")
}

fn codex_parent() -> PathBuf {
    fixtures()
        .join("codex/2026/01/06")
        .join(format!("rollout-2026-01-06T09-00-00-{A}.jsonl"))
}

fn codex_child() -> PathBuf {
    fixtures()
        .join("codex/2026/01/06")
        .join("rollout-2026-01-06T09-00-20-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl")
}

fn codex_legacy() -> PathBuf {
    fixtures()
        .join("codex/2025/06/01")
        .join(format!("rollout-2025-06-01T08-00-00-{C}.jsonl"))
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-import-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Every line of `path` through a fresh converter; the spans in creation
/// order and the counts.
fn convert(converter: &mut dyn Converter, path: &Path) -> (Vec<Span>, FileStats) {
    let mut stats = FileStats::default();
    read_lines(path, 0, converter, &mut stats).expect("reads");
    let spans = converter.spans().drain(0);
    (spans, stats)
}

fn claude(path: &Path) -> (Vec<Span>, FileStats) {
    convert(&mut claude_code::ClaudeCode::new(path, ""), path)
}

fn codex_with(path: &Path, ancestry: &codex::Ancestry) -> (Vec<Span>, FileStats) {
    convert(&mut codex::Codex::new(path, "", ancestry), path)
}

fn operation(span: &Span) -> Option<&str> {
    span.attributes
        .get("gen_ai.operation.name")
        .and_then(Value::as_str)
}

fn kind_count(spans: &[Span], kind: &str) -> usize {
    spans
        .iter()
        .filter(|span| operation(span) == Some(kind))
        .count()
}

fn turns(spans: &[Span]) -> Vec<&Span> {
    spans
        .iter()
        .filter(|span| span.name == TURN_SPAN_NAME)
        .collect()
}

fn by_id<'a>(spans: &'a [Span], id: &str) -> &'a Span {
    spans
        .iter()
        .find(|span| span.span_id == id)
        .unwrap_or_else(|| panic!("no span {id}"))
}

fn attr<'a>(span: &'a Span, key: &str) -> &'a Value {
    span.attributes.get(key).unwrap_or(&Value::Null)
}

fn llm(spans: &[Span], session: &str, message: &str) -> Span {
    by_id(spans, &span_id(Source::ClaudeCode, session, "llm", message)).clone()
}

fn tool(spans: &[Span], session: &str, call: &str) -> Span {
    by_id(spans, &span_id(Source::ClaudeCode, session, "tool", call)).clone()
}

#[test]
fn a_claude_code_session_becomes_turns_llm_steps_and_tool_steps() {
    let (spans, stats) = claude(&cc_main());
    assert_eq!(turns(&spans).len(), 2);
    assert_eq!(kind_count(&spans, "chat"), 7);
    assert_eq!(kind_count(&spans, "execute_tool"), 4);
    assert!(spans
        .iter()
        .all(|span| span.trace_id == trace_id(Source::ClaudeCode, S1)));

    // One API response written as three records counts its usage ONCE.
    let first = llm(&spans, S1, "msg_01");
    assert_eq!(attr(&first, "gen_ai.usage.input_tokens"), &json!(10));
    assert_eq!(attr(&first, "gen_ai.usage.output_tokens"), &json!(120));
    assert_eq!(
        attr(&first, "gen_ai.usage.cache_read.input_tokens"),
        &json!(1000)
    );
    assert_eq!(
        attr(&first, "gen_ai.usage.cache_creation.input_tokens"),
        &json!(200)
    );
    assert_eq!(attr(&first, "gen_ai.provider.name"), &json!("anthropic"));
    assert_eq!(
        attr(&first, "gen_ai.request.model"),
        &json!("claude-sonnet-4-5-20250929")
    );
    let output = &attr(&first, "gen_ai.output.messages")[0];
    let part_types: Vec<&str> = output["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .filter_map(|part| part["type"].as_str())
        .collect();
    assert_eq!(part_types, ["reasoning", "text", "tool_call"]);
    assert_eq!(output["finish_reason"], json!("tool_call"));
    // Its input is the prompt; the call started when the prompt was written.
    let input = attr(&first, "gen_ai.input.messages");
    assert_eq!(
        input[0]["parts"][0]["content"],
        json!("Add a total to the cart summary")
    );
    assert_eq!(first.start_time_ns, 1_767_607_200_000_000_000);
    assert_eq!(first.end_time_ns, 1_767_607_203_500_000_000);

    // The next call's input is the tool result delta.
    let second = llm(&spans, S1, "msg_02");
    assert_eq!(
        attr(&second, "gen_ai.input.messages")[0]["parts"][0]["type"],
        json!("tool_call_response")
    );

    // Tool steps hang under the response that requested them.
    let read = tool(&spans, S1, "toolu_01");
    assert_eq!(read.parent_span_id.as_deref(), Some(first.span_id.as_str()));
    assert_eq!(attr(&read, "gen_ai.tool.name"), &json!("Read"));
    assert_eq!(
        attr(&read, "gen_ai.tool.call.arguments"),
        &json!({"file_path": "/tmp/demo-shop/src/cart.rs"})
    );
    assert_eq!(
        attr(&read, "gen_ai.tool.call.result"),
        &json!("pub struct Cart { items: Vec<Item> }")
    );
    assert_eq!(read.status, "ok");
    assert_eq!(tool(&spans, S1, "toolu_03").status, "error", "is_error");

    // Turns carry the prompt and the session's base attributes.
    let first_turn = turns(&spans)[0].clone();
    assert_eq!(
        first.parent_span_id.as_deref(),
        Some(first_turn.span_id.as_str())
    );
    assert_eq!(first_turn.events[0].name, "gen_ai.user.message");
    assert_eq!(
        first_turn.events[0].attributes["content"],
        json!("Add a total to the cart summary")
    );
    assert_eq!(
        attr(&first_turn, "session.goal"),
        &json!("Add a total to the cart summary")
    );
    for span in &spans {
        assert_eq!(attr(span, "session.id"), &json!(S1), "{}", span.name);
        assert_eq!(attr(span, "traza.source"), &json!("claude-code"));
        assert_eq!(attr(span, "code.project"), &json!("/tmp/demo-shop"));
        assert_eq!(attr(span, "service.version"), &json!("2.1.0"));
        assert_eq!(span.service, "claude-code");
    }

    // A synthesized API error is a failed call of the model in use; the turn
    // takes the status of its LAST call, which recovered.
    let failed = llm(&spans, S1, "msg_06");
    assert_eq!(failed.status, "error");
    assert_eq!(attr(&failed, "error.type"), &json!("overloaded"));
    assert_eq!(
        attr(&failed, "gen_ai.request.model"),
        &json!("claude-sonnet-4-5-20250929")
    );
    assert_eq!(attr(&failed, "gen_ai.response.model"), &Value::Null);
    let second_turn = turns(&spans)[1].clone();
    assert_eq!(second_turn.status, "ok");
    assert!(second_turn
        .events
        .iter()
        .any(|event| event.name == "claude_code.api_error"));
    assert!(attr(&first_turn, "session.goal").is_string());
    assert_eq!(attr(&second_turn, "session.goal"), &Value::Null);

    // Unknown records and garbage lines are counted, never fatal.
    assert_eq!(stats.unknown_records.get("x-future-record"), Some(&1));
    assert_eq!(stats.malformed_lines, 1);
    assert_eq!(stats.records.get("summary"), Some(&1));
}

#[test]
fn a_subagent_transcript_is_an_agent_span_under_the_call_that_spawned_it() {
    let (spans, _) = claude(&cc_agent());
    let agent = spans
        .iter()
        .find(|span| operation(span) == Some("invoke_agent"))
        .expect("agent span");
    assert_eq!(
        agent.parent_span_id.as_deref(),
        Some(span_id(Source::ClaudeCode, S1, "tool", "toolu_04").as_str()),
        "derived from the meta file's toolUseId: the parent's Agent tool span"
    );
    assert_eq!(agent.trace_id, trace_id(Source::ClaudeCode, S1));
    assert_eq!(attr(agent, "gen_ai.agent.name"), &json!("general-purpose"));
    assert_eq!(
        attr(agent, "input.value"),
        &json!("Add a price field to Item in src/item.rs")
    );
    assert_eq!(
        attr(agent, "output.value"),
        &json!("Added `price: u64` to Item.")
    );
    let calls: Vec<&Span> = spans
        .iter()
        .filter(|span| operation(span) == Some("chat"))
        .collect();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.parent_span_id.as_deref(), Some(agent.span_id.as_str()));
        assert_eq!(attr(call, "gen_ai.agent.name"), &json!("general-purpose"));
        assert!(
            call.start_time_ns > agent.start_time_ns,
            "the agent sorts before its first step"
        );
    }
    let edit = tool(&spans, S1, "toolu_s1");
    assert_eq!(attr(&edit, "gen_ai.tool.name"), &json!("MultiEdit"));
    assert_eq!(
        edit.parent_span_id.as_deref(),
        Some(calls[0].span_id.as_str())
    );
}

#[test]
fn ids_are_derived_so_converting_again_is_byte_identical() {
    for path in [cc_main(), cc_agent()] {
        let (first, _) = claude(&path);
        let (second, _) = claude(&path);
        assert_eq!(first, second, "{}", path.display());
    }
    let ancestry = codex::Ancestry::default();
    assert_eq!(
        codex_with(&codex_parent(), &ancestry).0,
        codex_with(&codex_parent(), &ancestry).0
    );
    // The derivation is pinned: a change here re-keys every import.
    assert_eq!(
        span_id(Source::ClaudeCode, "s", "llm", "m"),
        derive_id(Source::ClaudeCode, "s", "llm", "m", 16)
    );
    assert_eq!(trace_id(Source::ClaudeCode, S1).len(), 32);
    assert_ne!(
        trace_id(Source::ClaudeCode, S1),
        trace_id(Source::Codex, S1),
        "sources are namespaced"
    );
}

#[test]
fn a_codex_rollout_groups_items_into_responses_with_usage_attached_once() {
    let (spans, stats) = codex_with(&codex_parent(), &codex::Ancestry::default());
    let calls: Vec<&Span> = spans
        .iter()
        .filter(|span| operation(span) == Some("chat"))
        .collect();
    assert_eq!(calls.len(), 8, "four in t1, two replayed in t2, two in t3");
    let usage: Vec<(Value, Value, Value)> = calls
        .iter()
        .map(|call| {
            (
                attr(call, "gen_ai.usage.input_tokens").clone(),
                attr(call, "gen_ai.usage.output_tokens").clone(),
                attr(call, "gen_ai.usage.cache_read.input_tokens").clone(),
            )
        })
        .collect();
    assert_eq!(
        usage,
        [
            (json!(1000), json!(100), json!(800)),
            (json!(1200), json!(100), json!(1000)),
            (json!(1250), json!(50), json!(1100)),
            (json!(150), json!(50), json!(0)),
            (json!(500), json!(60), json!(400)),
            (json!(600), json!(30), json!(500)),
            (json!(200), json!(20), json!(100)),
            (json!(100), json!(10), json!(0)),
        ]
    );
    assert_eq!(
        stats.records.get("token_count/content-unrecorded"),
        Some(&1),
        "only t3's orphan: a repeated token count (same total) is no new call"
    );
    assert_eq!(stats.records.get("item_completed/late"), Some(&1));
    for call in &calls {
        assert_eq!(attr(call, "gen_ai.request.model"), &json!("gpt-5-codex"));
        assert_eq!(attr(call, "gen_ai.provider.name"), &json!("openai"));
    }
    let turn = turns(&spans)[0].clone();
    assert_eq!(
        turn.events[0].attributes["content"],
        json!("Add a /health endpoint"),
        "injected <environment_context> is not the prompt"
    );
    assert_eq!(
        attr(&turn, "session.goal"),
        &json!("Add a /health endpoint")
    );
    let tool_named = |name: &str| {
        spans
            .iter()
            .find(|span| attr(span, "gen_ai.tool.name") == &json!(name))
            .unwrap_or_else(|| panic!("tool {name}"))
            .clone()
    };
    let patch = tool_named("apply_patch");
    assert!(attr(&patch, "gen_ai.tool.call.arguments")
        .as_str()
        .is_some_and(|text| text.starts_with("*** Begin Patch")));
    assert_eq!(attr(&patch, "code.cwd"), &json!("/tmp/demo-api"));
    assert_eq!(attr(&patch, "process.exit.code"), &json!(0));
    assert_eq!(patch.status, "ok");
    assert_eq!(
        patch.parent_span_id.as_deref(),
        Some(calls[1].span_id.as_str())
    );
    let commands: Vec<Span> = spans
        .iter()
        .filter(|span| attr(span, "gen_ai.tool.name") == &json!("exec_command"))
        .cloned()
        .collect();
    assert_eq!(
        attr(&commands[0], "gen_ai.tool.call.arguments"),
        &json!({"cmd": "rg -n router src", "workdir": "/tmp/demo-api"}),
        "JSON-string arguments are kept parsed"
    );
    assert_eq!(commands[1].status, "error", "exit code 1");
    assert_eq!(stats.unknown_records.get("some_future_type"), Some(&1));
    assert!(spans
        .iter()
        .all(|span| attr(span, "session.id") == &json!(A) && span.service == "codex"));
}

#[test]
fn an_item_only_turn_is_replayed_as_its_responses_and_tool_calls() {
    let (spans, _) = codex_with(&codex_parent(), &codex::Ancestry::default());
    let turn = turns(&spans)[1].clone();
    assert_eq!(
        turn.events[0].attributes["content"],
        json!("Document the API"),
        "the prompt came from a UserMessage item"
    );
    let children = |parent: &str| -> Vec<Span> {
        spans
            .iter()
            .filter(|span| span.parent_span_id.as_deref() == Some(parent))
            .cloned()
            .collect()
    };
    let calls = children(&turn.span_id);
    assert_eq!(calls.len(), 2);
    let first_parts = &attr(&calls[0], "gen_ai.output.messages")[0]["parts"];
    assert_eq!(first_parts[0]["type"], json!("reasoning"));
    assert_eq!(first_parts[0]["content"], json!("Write docs/API.md"));
    // The file change came after the first call's token count; it is still
    // that call's tool step, and its result is the second call's input.
    let tools = children(&calls[0].span_id);
    assert_eq!(tools.len(), 1);
    assert_eq!(attr(&tools[0], "gen_ai.tool.name"), &json!("apply_patch"));
    assert!(attr(&tools[0], "gen_ai.tool.call.arguments")["changes"]
        .get("/tmp/demo-api/docs/API.md")
        .is_some());
    assert_eq!(
        attr(&tools[0], "gen_ai.tool.call.result"),
        &json!("Success. Added docs/API.md")
    );
    assert_eq!(
        attr(&calls[1], "gen_ai.input.messages")[0]["parts"][0]["type"],
        json!("tool_call_response")
    );
    let output = &attr(&calls[1], "gen_ai.output.messages")[0];
    assert_eq!(output["finish_reason"], json!("stop"));
    assert_eq!(
        output["parts"][0]["content"],
        json!("Documented /health in docs/API.md.")
    );
}

#[test]
fn a_token_count_no_response_claims_is_a_call_whose_content_was_not_recorded() {
    let (spans, _) = codex_with(&codex_parent(), &codex::Ancestry::default());
    let turn = turns(&spans)[2].clone();
    let calls: Vec<&Span> = spans
        .iter()
        .filter(|span| span.parent_span_id.as_deref() == Some(turn.span_id.as_str()))
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(attr(calls[0], "codex.content_unrecorded"), &Value::Null);
    assert_eq!(attr(calls[1], "codex.content_unrecorded"), &json!(true));
    assert_eq!(attr(calls[1], "gen_ai.usage.input_tokens"), &json!(100));
    assert_eq!(attr(calls[1], "gen_ai.usage.output_tokens"), &json!(10));
}

#[test]
fn a_codex_subagent_thread_joins_its_roots_run() {
    let mut ancestry = codex::Ancestry::default();
    ancestry.learn(&discover(Source::Codex, &fixtures().join("codex")));
    let (spans, _) = codex_with(&codex_child(), &ancestry);
    let agent = spans
        .iter()
        .find(|span| operation(span) == Some("invoke_agent"))
        .expect("agent span");
    assert_eq!(attr(agent, "gen_ai.agent.name"), &json!("reviewer"));
    assert_eq!(agent.trace_id, trace_id(Source::Codex, A));
    for span in &spans {
        assert_eq!(
            attr(span, "session.id"),
            &json!(A),
            "joins the parent's run"
        );
    }
    let call = spans
        .iter()
        .find(|span| operation(span) == Some("chat"))
        .expect("call");
    assert_eq!(call.parent_span_id.as_deref(), Some(agent.span_id.as_str()));
    assert_eq!(
        attr(call, "gen_ai.request.model"),
        &json!("gpt-5-codex-mini")
    );
    assert!(
        turns(&spans).is_empty(),
        "a sub-agent's turns are its agent span"
    );
}

#[test]
fn the_oldest_rollout_shape_still_converts() {
    let (spans, stats) = codex_with(&codex_legacy(), &codex::Ancestry::default());
    assert_eq!(turns(&spans).len(), 1);
    assert_eq!(kind_count(&spans, "chat"), 2);
    let shell = spans
        .iter()
        .find(|span| attr(span, "gen_ai.tool.name") == &json!("shell"))
        .expect("shell");
    assert_eq!(attr(shell, "process.exit.code"), &json!(0));
    assert_eq!(attr(shell, "vcs.ref.head.name"), &json!("dev"));
    assert_eq!(stats.records.get("state"), Some(&1));
    assert!(stats.unknown_records.is_empty());
}

/// Resuming from an offset must send exactly what following the file live
/// would have sent past that offset — for every line boundary.
#[test]
fn resend_suppression_equals_live_following_at_every_cut() {
    for (path, make) in [
        (
            cc_main(),
            (|p: &Path| Box::new(claude_code::ClaudeCode::new(p, "")) as Box<dyn Converter>)
                as fn(&Path) -> Box<dyn Converter>,
        ),
        (codex_parent(), |p: &Path| {
            Box::new(codex::Codex::new(p, "", &codex::Ancestry::default())) as Box<dyn Converter>
        }),
    ] {
        let bytes = std::fs::read(&path).expect("fixture");
        let cuts: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .map(|(at, _)| at + 1)
            .collect();
        for &cut in &cuts {
            // Live: a converter that read up to the cut, then the rest.
            let mut live = make(&path);
            let mut stats = FileStats::default();
            read_lines_from(
                std::io::Cursor::new(&bytes[..cut]),
                0,
                live.as_mut(),
                &mut stats,
            )
            .expect("prefix");
            live.spans().drain(0);
            read_lines_from(
                std::io::Cursor::new(&bytes[cut..]),
                cut as u64,
                live.as_mut(),
                &mut stats,
            )
            .expect("rest");
            let followed = live.spans().drain(0);
            // Resumed: a fresh converter over everything, suppressing what
            // was sent before the cut.
            let mut fresh = make(&path);
            read_lines_from(
                std::io::Cursor::new(&bytes[..]),
                0,
                fresh.as_mut(),
                &mut stats,
            )
            .expect("whole");
            let resumed = fresh.spans().drain(cut as u64);
            let key = |spans: &[Span]| -> BTreeMap<String, Span> {
                spans
                    .iter()
                    .map(|span| (span.span_id.clone(), span.clone()))
                    .collect()
            };
            assert_eq!(
                key(&followed),
                key(&resumed),
                "{} cut at byte {cut}",
                path.display()
            );
        }
    }
}

/// A sink that keeps what it is sent, refusing any span `refuse` names.
struct Memory {
    sent: Vec<Span>,
    refuse: fn(&Span) -> bool,
}

impl Memory {
    fn new() -> Self {
        Self {
            sent: Vec::new(),
            refuse: |_| false,
        }
    }
}

impl Sink for Memory {
    fn send(&mut self, spans: &[Span]) -> Result<(), SendError> {
        if spans.iter().any(|span| (self.refuse)(span)) {
            return Err(SendError::Refused("refused by the test".into()));
        }
        self.sent.extend_from_slice(spans);
        Ok(())
    }
}

fn options(source: Source, root: &Path, state: Option<PathBuf>) -> ImportOptions {
    ImportOptions {
        source,
        root: root.to_path_buf(),
        tenant: None,
        since: None,
        project_filter: None,
        state,
        destination_key: "test".into(),
        watch: false,
        interval: Duration::from_millis(50),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn import(options: &ImportOptions, sink: &mut Memory) -> ImportStats {
    run(options, sink, &mut std::io::sink(), &|| true).expect("imports")
}

#[test]
fn importing_again_sends_nothing_and_appended_lines_send_only_what_they_touch() {
    let dir = scratch("resume");
    copy_tree(&fixtures().join("claude-code"), &dir);
    let state = dir.join("state.json");
    let options = options(Source::ClaudeCode, &dir, Some(state.clone()));

    let mut sink = Memory::new();
    let first = import(&options, &mut sink);
    assert_eq!((first.files_read, first.spans_sent), (3, 21));
    assert_eq!(first.sessions, 2);

    let again = import(&options, &mut sink);
    assert_eq!(
        again.spans_sent, 0,
        "the state file says everything was sent"
    );
    assert_eq!(again.files_unchanged, 3);

    // A new turn: its turn span and its one call — nothing earlier.
    let main = dir.join("-tmp-demo-shop").join(format!("{S1}.jsonl"));
    let prompt = json!({"type": "user", "uuid": "u9", "parentUuid": "a9", "sessionId": S1,
        "timestamp": "2026-01-05T10:10:00.000Z", "cwd": "/tmp/demo-shop",
        "message": {"role": "user", "content": "And add a test"}});
    let answer = json!({"type": "assistant", "uuid": "a10", "parentUuid": "u9", "sessionId": S1,
        "timestamp": "2026-01-05T10:10:04.000Z", "cwd": "/tmp/demo-shop",
        "message": {"id": "msg_09", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
            "content": [{"type": "text", "text": "Added."}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 4}}});
    let mut text = std::fs::read_to_string(&main).expect("read");
    text.push_str(&format!("{prompt}\n"));
    // Half a line: not consumed until it is whole.
    let line = answer.to_string();
    let (head, tail) = line.split_at(line.len() / 2);
    text.push_str(head);
    std::fs::write(&main, &text).expect("append");
    let mut sink = Memory::new();
    let partial = import(&options, &mut sink);
    assert_eq!(partial.spans_sent, 1, "the new turn only");
    assert_eq!(sink.sent[0].name, TURN_SPAN_NAME);

    text.push_str(tail);
    text.push('\n');
    std::fs::write(&main, &text).expect("complete");
    let mut sink = Memory::new();
    let completed = import(&options, &mut sink);
    let names: Vec<&str> = sink.sent.iter().map(|span| span.name.as_str()).collect();
    assert_eq!(
        names,
        ["turn", "chat claude-sonnet-4-5-20250929"],
        "the turn (extended) and its call"
    );
    assert_eq!(completed.spans_sent, 2);

    // Rewriting a file's first bytes makes it a new file: sent again whole.
    // (Noticed when the size moves: transcripts only ever grow, so an
    // idle tree costs one stat per file, not a read.)
    let other = dir.join("-tmp-demo-shop").join(format!("{S2}.jsonl"));
    let original = std::fs::read_to_string(&other).expect("read");
    std::fs::write(
        &other,
        original.replacen("flaky", "intermittently failing", 1),
    )
    .expect("rewrite");
    let mut sink = Memory::new();
    let rewritten = import(&options, &mut sink);
    assert_eq!(rewritten.spans_sent, 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_state_file_every_import_sends_everything_identically() {
    let options = options(Source::Codex, &fixtures().join("codex"), None);
    let mut first = Memory::new();
    let mut second = Memory::new();
    assert_eq!(import(&options, &mut first).spans_sent, 21);
    import(&options, &mut second);
    assert_eq!(
        first.sent, second.sent,
        "last write wins over identical spans"
    );
}

#[test]
fn a_refused_span_is_isolated_and_the_rest_still_land() {
    let options = options(Source::ClaudeCode, &fixtures().join("claude-code"), None);
    let mut sink = Memory::new();
    sink.refuse = |span| span.name == TURN_SPAN_NAME;
    let stats = import(&options, &mut sink);
    assert_eq!(stats.spans_refused, 3, "one per turn");
    assert_eq!(stats.spans_sent, 18);
    assert!(sink.sent.iter().all(|span| span.name != TURN_SPAN_NAME));
    assert!(stats.errors[0].contains("refused by the test"));
}

#[test]
fn filters_select_files_by_project_and_age() {
    let mut filtered = options(Source::ClaudeCode, &fixtures().join("claude-code"), None);
    filtered.project_filter = Some("no-such-project".into());
    let mut sink = Memory::new();
    let stats = import(&filtered, &mut sink);
    assert_eq!((stats.files_filtered, stats.spans_sent), (3, 0));

    filtered.project_filter = Some("/tmp/demo-shop".into()); // matches the cwd
    let stats = import(&filtered, &mut Memory::new());
    assert_eq!(stats.files_read, 3);

    let dir = scratch("since");
    copy_tree(&fixtures().join("codex"), &dir);
    let mut recent = options(Source::Codex, &dir, None);
    recent.since = Some(Duration::from_secs(3600));
    let stats = import(&recent, &mut Memory::new());
    assert_eq!(stats.files_read, 3, "just-copied files are recent");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A zstd frame of raw blocks: what a compressed rollout looks like to the
/// importer, built without an encoder.
fn raw_zstd(content: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x70];
    let blocks: Vec<&[u8]> = content.chunks(100_000).collect();
    for (index, block) in blocks.iter().enumerate() {
        let last = u32::from(index + 1 == blocks.len());
        let header = last | ((block.len() as u32) << 3);
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.extend_from_slice(block);
    }
    out
}

#[test]
fn a_compressed_rollout_continues_the_plain_file_it_replaced() {
    let dir = scratch("zst");
    let day = dir.join("2026/01/06");
    std::fs::create_dir_all(&day).expect("dir");
    let plain = day.join(format!("rollout-2026-01-06T09-00-00-{A}.jsonl"));
    let content = std::fs::read(codex_parent()).expect("fixture");
    std::fs::write(&plain, &content).expect("plain");
    let state = dir.join("state.json");
    let options = options(Source::Codex, &dir, Some(state));
    let mut sink = Memory::new();
    let first = import(&options, &mut sink);
    let sent = first.spans_sent;
    assert!(sent > 0);

    // Codex compresses the finished rollout in place.
    std::fs::remove_file(&plain).expect("remove");
    let compressed = day.join(format!("rollout-2026-01-06T09-00-00-{A}.jsonl.zst"));
    std::fs::write(&compressed, raw_zstd(&content)).expect("zst");
    let mut sink = Memory::new();
    let continued = import(&options, &mut sink);
    assert_eq!(
        continued.files_read, 1,
        "read once to learn where it stands"
    );
    assert_eq!(continued.spans_sent, 0, "nothing new in it");
    let again = import(&options, &mut Memory::new());
    assert_eq!(again.files_unchanged, 1, "now known by its compressed size");

    // A compressed rollout never seen plain is read whole.
    let fresh_dir = scratch("zst-fresh");
    std::fs::write(
        fresh_dir.join(format!("rollout-x-{A}.jsonl.zst")),
        raw_zstd(&content),
    )
    .expect("zst");
    let mut sink = Memory::new();
    let fresh = import(&self::options(Source::Codex, &fresh_dir, None), &mut sink);
    assert_eq!(fresh.spans_sent, sent, "the same spans as the plain file");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fresh_dir);
}

#[test]
fn text_is_bounded_at_character_boundaries() {
    assert_eq!(bounded("short", 10), "short");
    let cut = bounded("ééééé", 3);
    assert!(
        cut.starts_with('é') && cut.contains("8 more bytes"),
        "{cut}"
    );
    assert_eq!(preview("abcdef", 3), "abc…");
    assert_eq!(preview("abc", 3), "abc");
    let value = bounded_value(&json!({"a": ["x".repeat(20)], "n": 1}), 4);
    assert!(value["a"][0]
        .as_str()
        .is_some_and(|text| text.starts_with("xxxx…")));
    assert_eq!(value["n"], json!(1));
}

#[test]
fn an_oversized_line_is_skipped_and_counted() {
    let mut reader = std::io::Cursor::new(b"abc\ndef".to_vec());
    let mut line = Vec::new();
    assert_eq!(read_bounded_line(&mut reader, &mut line).expect("reads"), 4);
    assert_eq!(line, b"abc\n");
    line.clear();
    assert_eq!(read_bounded_line(&mut reader, &mut line).expect("reads"), 3);
    assert_eq!(line, b"def", "an unterminated tail stays unterminated");
}
