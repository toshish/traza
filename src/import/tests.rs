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
    convert(
        &mut codex::Codex::open(path, "", ancestry).expect("an identity"),
        path,
    )
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
            Box::new(codex::Codex::open(p, "", &codex::Ancestry::default()).expect("identity"))
                as Box<dyn Converter>
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

    // Ages are set explicitly: a copy may keep its source's modification
    // time (it does on macOS), so "just copied" says nothing about age.
    let dir = scratch("since");
    copy_tree(&fixtures().join("codex"), &dir);
    let files = discover(Source::Codex, &dir);
    let now = std::time::SystemTime::now();
    for (index, file) in files.iter().enumerate() {
        let age = if index == 0 {
            Duration::from_secs(2 * 86_400)
        } else {
            Duration::from_secs(60)
        };
        std::fs::File::options()
            .write(true)
            .open(&file.path)
            .and_then(|handle| handle.set_modified(now - age))
            .expect("sets the modification time");
    }
    let mut recent = options(Source::Codex, &dir, None);
    recent.since = Some(Duration::from_secs(3600));
    let stats = import(&recent, &mut Memory::new());
    assert_eq!(
        (stats.files_read, stats.files_filtered),
        (2, 1),
        "the file modified two days ago is outside the last hour"
    );
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
fn lines_are_read_whole_and_an_unterminated_tail_says_so() {
    let mut reader = std::io::Cursor::new(b"abc\ndef".to_vec());
    let mut line = Vec::new();
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES).expect("reads"),
        (4, true)
    );
    assert_eq!(line, b"abc\n");
    line.clear();
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES).expect("reads"),
        (3, false)
    );
    assert_eq!(line, b"def", "an unterminated tail stays unterminated");
}

/// A converter that only remembers the lines it was fed.
#[derive(Default)]
struct Lines {
    fed: Vec<(String, u64)>,
    spans: Spans,
}

impl Converter for Lines {
    fn feed(&mut self, line: &[u8], end: u64, _stats: &mut FileStats) {
        self.fed
            .push((String::from_utf8_lossy(line).into_owned(), end));
    }
    fn spans(&mut self) -> &mut Spans {
        &mut self.spans
    }
    fn run(&self) -> Option<String> {
        None
    }
}

#[test]
fn an_oversized_line_is_skipped_and_counted_and_the_lines_after_it_still_land() {
    // A reader whose buffer is far smaller than the line, as a 256 KiB
    // BufReader is against a 64 MiB cap: the newline arrives many chunks
    // after the cap was crossed.
    let text = format!(
        "{{\"a\":1}}\n{{\"big\":\"{}\"}}\n{{\"b\":2}}\n",
        "x".repeat(30)
    );
    for capacity in [1, 4, 7, 64, 4096] {
        let reader = BufReader::with_capacity(capacity, std::io::Cursor::new(text.clone()));
        let mut converter = Lines::default();
        let mut stats = FileStats::default();
        let consumed = read_lines_capped(reader, 0, &mut converter, &mut stats, 10).expect("reads");
        assert_eq!(
            consumed,
            text.len() as u64,
            "buffer {capacity}: read to the end, not stuck at the long line"
        );
        let fed: Vec<&str> = converter
            .fed
            .iter()
            .map(|(line, _)| line.as_str())
            .collect();
        assert_eq!(fed, ["{\"a\":1}", "{\"b\":2}"], "buffer {capacity}");
        assert_eq!(
            converter.fed[1].1,
            text.len() as u64,
            "the line after it ends where the file does"
        );
        assert_eq!(
            (stats.lines, stats.malformed_lines),
            (3, 1),
            "buffer {capacity}"
        );
    }

    // A line exactly at the cap is kept; one byte over is not.
    let at_cap = format!("{{\"s\":\"{}\"}}\n", "y".repeat(2)); // 10 bytes
    let mut converter = Lines::default();
    let mut stats = FileStats::default();
    read_lines_capped(
        BufReader::with_capacity(3, std::io::Cursor::new(at_cap)),
        0,
        &mut converter,
        &mut stats,
        10,
    )
    .expect("reads");
    assert_eq!(converter.fed.len(), 1);
    assert_eq!(stats.malformed_lines, 0);

    // An over-long tail with no newline yet waits for it.
    let tail = format!("{{\"a\":1}}\n{{\"big\":\"{}", "x".repeat(30));
    let mut converter = Lines::default();
    let mut stats = FileStats::default();
    let consumed = read_lines_capped(
        BufReader::with_capacity(4, std::io::Cursor::new(tail)),
        0,
        &mut converter,
        &mut stats,
        10,
    )
    .expect("reads");
    assert_eq!(consumed, 8, "stops before the unfinished line");
    assert_eq!(stats.malformed_lines, 0, "not judged until it ends");
}

/// Sets `path`'s modification time to `age` ago.
fn age(path: &Path, age: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|handle| handle.set_modified(std::time::SystemTime::now() - age))
        .expect("sets the modification time");
}

/// `count` one-turn Claude Code sessions in one project directory, each
/// modified `ago`.
fn many_sessions(label: &str, count: usize, ago: Duration) -> PathBuf {
    let dir = scratch(label);
    let project = dir.join("-tmp-many");
    std::fs::create_dir_all(&project).expect("dir");
    let template = std::fs::read(
        fixtures()
            .join("claude-code")
            .join("-tmp-demo-shop")
            .join(format!("{S2}.jsonl")),
    )
    .expect("fixture");
    for index in 0..count {
        let path = project.join(format!("00000000-0000-4000-8000-{index:012}.jsonl"));
        std::fs::write(&path, &template).expect("write");
        age(&path, ago);
    }
    dir
}

#[test]
fn watching_a_whole_history_holds_at_most_the_live_cap_of_converters() {
    // Seventy sessions written to just now: the first pass keeps no more
    // than the cap, not one converter per file.
    let dir = many_sessions("live-cap", MAX_LIVE_CONVERTERS + 6, Duration::ZERO);
    let mut watching = options(Source::ClaudeCode, &dir, None);
    watching.watch = true;
    let stats = import(&watching, &mut Memory::new());
    assert_eq!(stats.files_read, (MAX_LIVE_CONVERTERS + 6) as u64);
    assert_eq!(stats.peak_live_converters, MAX_LIVE_CONVERTERS);
    let _ = std::fs::remove_dir_all(&dir);

    // A history nobody is writing to keeps none: every file was last
    // modified longer ago than a live session would be.
    let dir = many_sessions("live-idle", 8, LIVE_IDLE + Duration::from_secs(60));
    let mut watching = options(Source::ClaudeCode, &dir, None);
    watching.watch = true;
    let stats = import(&watching, &mut Memory::new());
    assert_eq!(stats.files_read, 8);
    assert_eq!(stats.peak_live_converters, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn since_selects_whole_sessions_not_files() {
    // A Claude Code session whose main transcript changed recently, but
    // whose sub-agent wrote its last line days ago: both are read.
    let dir = scratch("since-session");
    copy_tree(&fixtures().join("claude-code"), &dir);
    let project = dir.join("-tmp-demo-shop");
    age(
        &project.join(format!("{S1}.jsonl")),
        Duration::from_secs(60),
    );
    age(
        &project
            .join(S1)
            .join("subagents")
            .join("agent-a1b2c3.jsonl"),
        Duration::from_secs(3 * 86_400),
    );
    age(
        &project.join(format!("{S2}.jsonl")),
        Duration::from_secs(3 * 86_400),
    );
    let mut recent = options(Source::ClaudeCode, &dir, None);
    recent.since = Some(Duration::from_secs(3600));
    let mut sink = Memory::new();
    let stats = import(&recent, &mut sink);
    assert_eq!(
        (stats.files_read, stats.files_filtered),
        (2, 1),
        "S1's transcript and its sub-agent's; S2 is old"
    );
    assert!(
        sink.sent
            .iter()
            .any(|span| operation(span) == Some("invoke_agent")),
        "the Agent tool call has its agent"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // A Codex sub-agent thread active now pulls in its older parent rollout,
    // so its run has its root.
    let dir = scratch("since-thread");
    copy_tree(&fixtures().join("codex"), &dir);
    for file in discover(Source::Codex, &dir) {
        let recent = file.path.to_string_lossy().contains("bbbbbbbb");
        age(
            &file.path,
            if recent {
                Duration::from_secs(60)
            } else {
                Duration::from_secs(3 * 86_400)
            },
        );
    }
    let mut recent = options(Source::Codex, &dir, None);
    recent.since = Some(Duration::from_secs(3600));
    let stats = import(&recent, &mut Memory::new());
    assert_eq!(
        (stats.files_read, stats.files_filtered),
        (2, 1),
        "the child, its parent; not the unrelated legacy session"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sink that is unreachable for its first `down` sends.
struct Flaky {
    down: usize,
    sent: Vec<Span>,
}

impl Sink for Flaky {
    fn send(&mut self, spans: &[Span]) -> Result<(), SendError> {
        if self.down > 0 {
            self.down -= 1;
            return Err(SendError::Unavailable("connection refused".into()));
        }
        self.sent.extend_from_slice(spans);
        Ok(())
    }
}

#[test]
fn a_watch_waits_out_an_unreachable_destination_and_loses_nothing() {
    let dir = scratch("outage");
    copy_tree(&fixtures().join("claude-code"), &dir);
    let state = dir.join("state.json");
    let mut watching = options(Source::ClaudeCode, &dir, Some(state.clone()));
    watching.watch = true;
    watching.interval = Duration::from_millis(10);
    let mut sink = Flaky {
        down: 3,
        sent: Vec::new(),
    };
    let calls = std::cell::Cell::new(0);
    let stop = || {
        calls.set(calls.get() + 1);
        calls.get() >= 16
    };
    let mut log = Vec::new();
    let stats = run(&watching, &mut sink, &mut log, &stop).expect("a watch survives an outage");
    let log = String::from_utf8_lossy(&log);
    assert!(log.contains("the destination is unavailable"), "{log}");
    assert!(log.contains("the destination is back"), "{log}");
    let unique: std::collections::BTreeSet<(String, String)> = sink
        .sent
        .iter()
        .map(|span| (span.trace_id.clone(), span.span_id.clone()))
        .collect();
    assert_eq!(
        unique.len(),
        21,
        "every span landed once the server was back"
    );
    assert_eq!(
        stats.files_read, 3,
        "each file counted once, not per attempt"
    );
    let saved = state::State::load(&state).expect("state");
    for file in discover(Source::ClaudeCode, &dir) {
        let key = file.path.to_string_lossy().into_owned();
        assert_eq!(
            saved.file("test", &key).map(|recorded| recorded.offset),
            Some(file.size),
            "{key} recorded as sent in full"
        );
    }

    // A batch import does not wait: it stops, saying why.
    let mut batch = options(Source::ClaudeCode, &dir, None);
    batch.watch = false;
    let mut sink = Flaky {
        down: 1,
        sent: Vec::new(),
    };
    let error =
        run(&batch, &mut sink, &mut std::io::sink(), &|| true).expect_err("a batch import stops");
    assert!(error.contains("connection refused"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        retry_wait(Duration::from_millis(10), 1),
        Duration::from_millis(20)
    );
    assert_eq!(retry_wait(Duration::from_secs(1), 30), MAX_RETRY_WAIT);
    assert_eq!(
        retry_wait(Duration::from_secs(60), 1),
        Duration::from_secs(60),
        "never shorter than the poll interval"
    );
}

#[test]
fn a_subagent_meta_file_written_late_still_parents_its_agent_span() {
    let dir = scratch("late-meta");
    let subagents = dir.join("-tmp-demo-shop").join(S1).join("subagents");
    std::fs::create_dir_all(&subagents).expect("dir");
    let transcript = subagents.join("agent-a1b2c3.jsonl");
    std::fs::copy(cc_agent(), &transcript).expect("copy");
    age(&transcript, Duration::ZERO); // a live sub-agent
    let meta_source = cc_agent().with_extension("meta.json");
    let meta = transcript.with_extension("meta.json");
    let mut watching = options(Source::ClaudeCode, &dir, None);
    watching.watch = true;
    watching.interval = Duration::from_millis(10);
    let mut sink = Memory::new();
    let calls = std::cell::Cell::new(0);
    // After the first pass, Claude Code writes the meta file; the
    // transcript itself does not grow again.
    let stop = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            std::fs::copy(&meta_source, &meta).expect("meta");
        }
        calls.get() >= 6
    };
    run(&watching, &mut sink, &mut std::io::sink(), &stop).expect("watches");
    let agent_id = span_id(Source::ClaudeCode, S1, "agent", "a1b2c3");
    let versions: Vec<&Span> = sink
        .sent
        .iter()
        .filter(|span| span.span_id == agent_id)
        .collect();
    assert!(versions.len() >= 2, "sent, then sent again once placed");
    assert_eq!(versions[0].parent_span_id, None, "no meta yet: unplaced");
    let last = versions[versions.len() - 1];
    assert_eq!(
        last.parent_span_id.as_deref(),
        Some(span_id(Source::ClaudeCode, S1, "tool", "toolu_04").as_str()),
        "re-parented under the Agent tool call once the meta file appeared"
    );
    assert_eq!(attr(last, "gen_ai.agent.name"), &json!("general-purpose"));
    assert_eq!(
        attr(last, "gen_ai.agent.description"),
        &json!("Fix the Item type")
    );
    // The agent's own calls are renamed too, last write winning.
    let calls_renamed = sink
        .sent
        .iter()
        .rev()
        .filter(|span| operation(span) == Some("chat"))
        .take(2)
        .all(|span| attr(span, "gen_ai.agent.name") == &json!("general-purpose"));
    assert!(calls_renamed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_patch_applied_through_a_shell_is_marked_as_an_edit() {
    let dir = scratch("shell-patch");
    let rollout = dir.join(format!("rollout-2025-06-02T08-00-00-{C}.jsonl"));
    let patch = "*** Begin Patch\n*** Add File: src/new.py\n+x = 1\n*** End Patch\n";
    let lines = [
        json!({"timestamp": "2025-06-02T08:00:00.000Z", "type": "session_meta",
               "payload": {"id": C, "cwd": "/repo", "cli_version": "0.1.0"}}),
        json!({"timestamp": "2025-06-02T08:00:01.000Z", "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "Add new.py"}]}}),
        json!({"timestamp": "2025-06-02T08:00:02.000Z", "type": "response_item",
               "payload": {"type": "function_call", "name": "shell", "call_id": "c1",
                           "arguments": json!({"command": ["apply_patch", patch],
                                               "workdir": "/repo/svc"}).to_string()}}),
        json!({"timestamp": "2025-06-02T08:00:03.000Z", "type": "response_item",
               "payload": {"type": "function_call", "name": "shell", "call_id": "c2",
                           "arguments": json!({"command": ["ls"]}).to_string()}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&rollout, text).expect("write");
    let (spans, _) = codex_with(&rollout, &codex::Ancestry::default());
    let call = |id: &str| -> Span {
        spans
            .iter()
            .find(|span| attr(span, "gen_ai.tool.call.id") == &json!(id))
            .unwrap_or_else(|| panic!("no call {id}"))
            .clone()
    };
    let marked = call("c1");
    assert_eq!(attr(&marked, crate::blame::EDIT_MARKER), &json!(true));
    let plain = call("c2");
    assert!(
        !plain.attributes.contains_key(crate::blame::EDIT_MARKER),
        "a command that carries no patch is not an edit"
    );
    let edits = crate::blame::edits(&marked);
    assert_eq!(edits[0].file, "/repo/svc/src/new.py");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A model call whose response had hundreds of large parts: 26 MB as
/// structured JSON, which the payload store does not offload.
fn huge_span() -> Span {
    let mut span = blank_span(
        Source::Codex,
        "",
        &trace_id(Source::Codex, "huge"),
        &span_id(Source::Codex, "huge", "llm", "1"),
        None,
        "chat gpt-5-codex".into(),
        1,
    );
    let parts: Vec<Value> = (0..400)
        .map(|index| json!({"type": "text", "content": format!("{index:03}{}", "x".repeat(64 * 1024))}))
        .collect();
    span.attributes.insert(
        "gen_ai.output.messages".into(),
        json!([{"role": "assistant", "parts": parts}]),
    );
    let fields: Map<String, Value> = (0..10)
        .map(|index| (format!("f{index}"), json!("y".repeat(512 * 1024))))
        .collect();
    span.attributes
        .insert("gen_ai.input.messages".into(), Value::Object(fields));
    span.attributes
        .insert("gen_ai.operation.name".into(), json!("chat"));
    span
}

#[test]
fn a_span_too_large_for_the_store_is_trimmed_not_refused() {
    let mut span = huge_span();
    assert!(fit_span(&mut span));
    let size = |key: &str| serde_json::to_vec(&span.attributes[key]).map_or(0, |v| v.len());
    let output = size("gen_ai.output.messages");
    let input = size("gen_ai.input.messages");
    assert!(
        output <= MAX_STRUCTURED_ATTRIBUTE_BYTES + 4096,
        "one attribute within its budget: {output}"
    );
    assert!(
        output + input <= MAX_STRUCTURED_BYTES + 8192,
        "the span's structured attributes within theirs: {output} + {input}"
    );
    let parts = span.attributes["gen_ai.output.messages"][0]["parts"]
        .as_array()
        .expect("still an array of parts");
    assert!(parts.len() > 10, "the first parts are kept whole");
    assert!(
        parts[parts.len() - 1]
            .as_str()
            .is_some_and(|marker| marker.contains("more items not kept")),
        "the cut is marked"
    );
    assert_eq!(attr(&span, "gen_ai.operation.name"), &json!("chat"));
    let small = blank_span(Source::Codex, "", "t", "s", None, "n".into(), 1);
    let mut unchanged = small.clone();
    assert!(!fit_span(&mut unchanged));
    assert_eq!(unchanged, small);

    // Through a real store with its default caps: accepted, counted trimmed.
    let dir = scratch("trim");
    let store = crate::Store::open(
        &dir,
        crate::Config {
            payload_threshold: Some(256 * 1024),
            ..crate::Config::default()
        },
    )
    .expect("store");
    let mut sink = StoreSink::new(store);
    let mut stats = ImportStats::default();
    send_all(&mut sink, vec![huge_span()], &mut stats).expect("sends");
    assert_eq!(
        (stats.spans_sent, stats.spans_refused, stats.spans_trimmed),
        (1, 0, 1),
        "{:?}",
        stats.errors
    );
    sink.finish().expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The text parts of a span's `gen_ai.input.messages`, in order.
fn input_texts(span: &Span) -> Vec<String> {
    attr(span, "gen_ai.input.messages")
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .flat_map(|message| message["parts"].as_array().cloned().unwrap_or_default())
                .filter_map(|part| part["content"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_model_call_names_its_conversation_and_records_a_delta_of_it() {
    // A main transcript is the session's conversation; a sub-agent's is its
    // own, so the X-Ray rebuilds each from its own calls and never pairs a
    // sub-agent's call with its parent's.
    let (main, _) = claude(&cc_main());
    let (agent, _) = claude(&cc_agent());
    let chats = |spans: &[Span]| -> Vec<Span> {
        spans
            .iter()
            .filter(|span| operation(span) == Some("chat"))
            .cloned()
            .collect()
    };
    let (main, agent) = (chats(&main), chats(&agent));
    assert!(
        main.len() >= 4 && agent.len() == 2,
        "{} {}",
        main.len(),
        agent.len()
    );
    for span in &main {
        assert_eq!(attr(span, crate::xray::CONVERSATION_ATTRIBUTE), &json!(S1));
        assert_eq!(attr(span, crate::xray::INPUT_DELTA_ATTRIBUTE), &json!(true));
    }
    for span in &agent {
        assert_eq!(
            attr(span, crate::xray::CONVERSATION_ATTRIBUTE),
            &json!(format!("{S1}/a1b2c3"))
        );
        assert_eq!(attr(span, crate::xray::INPUT_DELTA_ATTRIBUTE), &json!(true));
    }
    // Codex: each thread is its own conversation.
    let ancestry = codex::Ancestry::default();
    let (parent, _) = codex_with(&codex_parent(), &ancestry);
    let threads: std::collections::BTreeSet<String> = chats(&parent)
        .iter()
        .map(|span| {
            assert_eq!(attr(span, crate::xray::INPUT_DELTA_ATTRIBUTE), &json!(true));
            attr(span, crate::xray::CONVERSATION_ATTRIBUTE)
                .as_str()
                .expect("named")
                .to_owned()
        })
        .collect();
    assert_eq!(threads.into_iter().collect::<Vec<_>>(), [A.to_owned()]);
}

/// A Claude Code transcript with a compaction in the middle; `summary_first`
/// writes the summary before the boundary instead of after it.
fn compacted_transcript(dir: &Path, summary_first: bool) -> PathBuf {
    let path = dir.join(format!("{S2}.jsonl"));
    let record = |uuid: &str, second: u32, body: Value| -> String {
        let mut record = json!({
            "parentUuid": null, "isSidechain": false, "cwd": "/tmp/demo-shop",
            "sessionId": S2, "version": "2.1.0", "uuid": uuid,
            "timestamp": format!("2026-01-05T12:00:{second:02}.000Z"),
        });
        for (key, value) in body.as_object().expect("object") {
            record[key] = value.clone();
        }
        record.to_string()
    };
    let reply = |id: &str, text: &str| {
        json!({"type": "assistant", "message": {
            "id": id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929",
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 5, "output_tokens": 5}}})
    };
    let boundary = record(
        "b1",
        10,
        json!({"type": "system", "subtype": "compact_boundary",
               "compactMetadata": {"trigger": "auto", "preTokens": 150000}}),
    );
    let summary = record(
        "s1",
        11,
        json!({"type": "user", "isCompactSummary": true,
               "message": {"role": "user", "content": "Summary: the login test was rewritten."}}),
    );
    let mut lines = vec![
        record(
            "u1",
            0,
            json!({"type": "user", "message": {"role": "user",
                   "content": "Never touch the migrations folder"}}),
        ),
        record("a1", 2, reply("msg_a", "Understood.")),
        record(
            "u2",
            4,
            json!({"type": "user", "message": {"role": "user", "content": "Fix the login test"}}),
        ),
    ];
    if summary_first {
        lines.extend([summary, boundary]);
    } else {
        lines.extend([boundary, summary]);
    }
    lines.extend([
        record(
            "u3",
            20,
            json!({"type": "user", "message": {"role": "user", "content": "Now run it"}}),
        ),
        record("a2", 22, reply("msg_b", "It passes.")),
        record(
            "u4",
            30,
            json!({"type": "user", "message": {"role": "user", "content": "Thanks"}}),
        ),
        record("a3", 32, reply("msg_c", "You're welcome.")),
    ]);
    std::fs::write(&path, lines.join("\n") + "\n").expect("writes");
    path
}

#[test]
fn the_first_call_after_a_compaction_starts_its_conversation_over() {
    for summary_first in [false, true] {
        let dir = scratch("compaction");
        let path = compacted_transcript(&dir, summary_first);
        let (spans, stats) = claude(&path);
        let (before, after, later) = (
            llm(&spans, S2, "msg_a"),
            llm(&spans, S2, "msg_b"),
            llm(&spans, S2, "msg_c"),
        );
        assert_eq!(
            attr(&before, crate::xray::INPUT_DELTA_ATTRIBUTE),
            &json!(true)
        );
        // The call after the compaction holds its whole context — the summary
        // and what followed — and nothing from before the boundary: "Fix the
        // login test" was summarized away with the rest.
        assert_eq!(
            attr(&after, crate::xray::INPUT_DELTA_ATTRIBUTE),
            &json!(false),
            "summary first: {summary_first}"
        );
        assert_eq!(
            input_texts(&after),
            ["Summary: the login test was rewritten.", "Now run it"],
            "summary first: {summary_first}"
        );
        // The next call is a delta of the restarted conversation again.
        assert_eq!(
            attr(&later, crate::xray::INPUT_DELTA_ATTRIBUTE),
            &json!(true)
        );
        assert_eq!(input_texts(&later), ["Thanks"]);
        assert_eq!(stats.records.get("system/compact_boundary"), Some(&1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_codex_compaction_restarts_from_its_replacement_history() {
    let dir = scratch("codex-compaction");
    let rollout = dir.join(format!("rollout-2025-06-03T08-00-00-{C}.jsonl"));
    let line = |second: u32, kind: &str, payload: Value| -> String {
        json!({"timestamp": format!("2025-06-03T08:00:{second:02}.000Z"),
               "type": kind, "payload": payload})
        .to_string()
    };
    let user = |text: &str| {
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": text}]})
    };
    let answer = |text: &str| {
        json!({"type": "message", "role": "assistant", "phase": "final_answer",
               "content": [{"type": "output_text", "text": text}]})
    };
    let tokens = |input: u64| {
        json!({"type": "token_count", "info": {"total_token_usage": {
            "input_tokens": input, "cached_input_tokens": 0, "output_tokens": 10,
            "reasoning_output_tokens": 0, "total_tokens": input + 10},
            "last_token_usage": {"input_tokens": input, "cached_input_tokens": 0,
            "output_tokens": 10, "reasoning_output_tokens": 0, "total_tokens": input + 10}}})
    };
    let lines = [
        line(
            0,
            "session_meta",
            json!({"id": C, "timestamp": "2025-06-03T08:00:00.000Z", "cwd": "/tmp/demo-api",
                   "originator": "codex_cli_rs", "cli_version": "0.150.0",
                   "model_provider": "openai"}),
        ),
        line(
            1,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t1"}),
        ),
        line(
            1,
            "turn_context",
            json!({"turn_id": "t1", "cwd": "/tmp/demo-api", "model": "gpt-5-codex"}),
        ),
        line(2, "response_item", user("Keep the public API stable")),
        line(3, "response_item", answer("Noted.")),
        line(3, "event_msg", tokens(100)),
        line(
            4,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t1"}),
        ),
        line(
            5,
            "compacted",
            json!({"message": "", "replacement_history": [
                user("Summary: the API is frozen."),
                {"type": "reasoning", "summary": []},
            ]}),
        ),
        line(
            6,
            "event_msg",
            json!({"type": "task_started", "turn_id": "t2"}),
        ),
        line(
            6,
            "turn_context",
            json!({"turn_id": "t2", "cwd": "/tmp/demo-api", "model": "gpt-5-codex"}),
        ),
        line(7, "response_item", user("Add a health route")),
        line(8, "response_item", answer("Added.")),
        line(8, "event_msg", tokens(250)),
        line(
            9,
            "event_msg",
            json!({"type": "task_complete", "turn_id": "t2"}),
        ),
    ];
    std::fs::write(&rollout, lines.join("\n") + "\n").expect("writes");
    let ancestry = codex::Ancestry::default();
    let (spans, stats) = codex_with(&rollout, &ancestry);
    let chats: Vec<&Span> = spans
        .iter()
        .filter(|span| operation(span) == Some("chat"))
        .collect();
    assert_eq!(chats.len(), 2, "{stats:?}");
    let (first, second) = (chats[0], chats[1]);
    assert_eq!(
        attr(first, crate::xray::INPUT_DELTA_ATTRIBUTE),
        &json!(true)
    );
    assert_eq!(
        attr(second, crate::xray::INPUT_DELTA_ATTRIBUTE),
        &json!(false)
    );
    assert_eq!(
        input_texts(second),
        ["Summary: the API is frozen.", "Add a health route"]
    );
    assert_eq!(attr(second, crate::xray::CONVERSATION_ATTRIBUTE), &json!(C));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_response_repeated_with_endless_new_blocks_holds_bounded_dedup_state() {
    // A malformed transcript: one message id, record after record, each
    // with a text block never seen before. One span; the digests remembered
    // to recognise a repeat must not grow with the file.
    let path = Path::new("/nowhere").join(format!("{S2}.jsonl"));
    let mut converter = claude_code::ClaudeCode::new(&path, "");
    let mut stats = FileStats::default();
    let records = claude_code::MAX_SEEN_BLOCKS + 500;
    let mut end = 0u64;
    for index in 0..records {
        let line = cc_record(
            &format!("a{index}"),
            1,
            json!({"type": "assistant", "message": {
                "id": "msg_same", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [{"type": "text", "text": format!("block {index}")}]}}),
        );
        end += line.len() as u64 + 1;
        converter.feed(line.as_bytes(), end, &mut stats);
    }
    assert!(
        converter.dedup_entries() <= claude_code::MAX_SEEN_BLOCKS,
        "{} digests held for {records} blocks",
        converter.dedup_entries()
    );
    assert_eq!(
        stats.records.get("dedup/forgotten"),
        Some(&500),
        "what was forgotten is counted"
    );
    assert_eq!(
        converter.spans().len(),
        2,
        "the implicit turn and one response"
    );
    // A block repeated within the window is still recognised.
    let repeat = cc_record(
        "again",
        2,
        json!({"type": "assistant", "message": {
            "id": "msg_same", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": format!("block {}", records - 1)}]}}),
    );
    let before = stats.clone();
    converter.feed(repeat.as_bytes(), end + 1000, &mut stats);
    assert_eq!(
        stats.records.get("dedup/forgotten"),
        before.records.get("dedup/forgotten")
    );
    assert_eq!(converter.dedup_entries(), claude_code::MAX_SEEN_BLOCKS);
}

#[test]
fn completions_of_turns_that_never_started_hold_bounded_state() {
    let path = Path::new("/nowhere").join(format!("rollout-2025-06-04T08-00-00-{C}.jsonl"));
    let mut converter = codex::Codex::new(&path, "", &codex::Ancestry::default(), None);
    let mut stats = FileStats::default();
    let completions = 3 * codex::MAX_CLOSED_TURNS;
    let mut end = 0u64;
    for index in 0..completions {
        let line = json!({"timestamp": "2025-06-04T08:00:01.000Z", "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": format!("never-started-{index}")}})
        .to_string();
        end += line.len() as u64 + 1;
        converter.feed(line.as_bytes(), end, &mut stats);
    }
    assert!(
        converter.closed_turns_len() <= codex::MAX_CLOSED_TURNS,
        "{} turn ids held for {completions} completions",
        converter.closed_turns_len()
    );
    assert_eq!(
        stats.records.get("closed_turns/forgotten"),
        Some(&(2 * codex::MAX_CLOSED_TURNS as u64))
    );
    // The most recent ones are still recognised: an item naming one is late.
    let late = json!({"timestamp": "2025-06-04T08:00:02.000Z", "type": "event_msg",
        "payload": {"type": "item_completed", "turn_id": format!("never-started-{}", completions - 1),
                    "item": {"type": "AgentMessage", "id": "x"}}})
    .to_string();
    converter.feed(late.as_bytes(), end + 1000, &mut stats);
    assert_eq!(stats.records.get("item_completed/late"), Some(&1));
}

#[test]
fn items_held_for_a_turn_are_bounded_by_bytes_as_well_as_count() {
    let path = Path::new("/nowhere").join(format!("rollout-2025-06-04T08-00-00-{C}.jsonl"));
    let budget = 8 * 1024;
    let mut converter =
        codex::Codex::new(&path, "", &codex::Ancestry::default(), None).with_pending_budget(budget);
    let mut stats = FileStats::default();
    let start = json!({"timestamp": "2025-06-04T08:00:00.000Z", "type": "event_msg",
        "payload": {"type": "task_started", "turn_id": "t1"}})
    .to_string();
    converter.feed(start.as_bytes(), 100, &mut stats);
    for index in 0..64u64 {
        let line = json!({"timestamp": "2025-06-04T08:00:01.000Z", "type": "event_msg",
            "payload": {"type": "item_completed", "turn_id": "t1",
                "item": {"type": "CommandExecution", "id": format!("i{index}"),
                         "command": "cat big", "aggregated_output": "x".repeat(1024)}}})
        .to_string();
        converter.feed(line.as_bytes(), 200 + index, &mut stats);
    }
    assert!(
        converter.pending_bytes() <= budget,
        "{} bytes held against a budget of {budget}",
        converter.pending_bytes()
    );
    assert!(stats
        .records
        .get("item_completed/overflow")
        .is_some_and(|n| *n > 0));
}

/// The Codex fixtures copied under a scratch directory, with the child
/// rollout cut half-way through its first (meta) line, as a watch finds a
/// rollout Codex has only begun to write. Returns the directory, the child's
/// path and its whole content.
fn codex_with_partial_child(label: &str) -> (PathBuf, PathBuf, Vec<u8>) {
    let dir = scratch(label);
    let day = dir.join("2026/01/06");
    std::fs::create_dir_all(&day).expect("dir");
    std::fs::copy(
        codex_parent(),
        day.join(codex_parent().file_name().expect("name")),
    )
    .expect("parent");
    let child = day.join(codex_child().file_name().expect("name"));
    let content = std::fs::read(codex_child()).expect("fixture");
    let meta_end = content
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a first line");
    std::fs::write(&child, &content[..meta_end / 2]).expect("partial");
    (dir, child, content)
}

#[test]
fn a_rollout_whose_meta_line_was_incomplete_is_learned_once_it_is_whole() {
    let (dir, child, content) = codex_with_partial_child("learn-retry");
    let mut ancestry = codex::Ancestry::default();
    ancestry.learn(&discover(Source::Codex, &dir));
    assert_eq!(ancestry.root_of(&child), None, "nothing to learn yet");
    std::fs::write(&child, &content).expect("whole");
    ancestry.learn(&discover(Source::Codex, &dir));
    assert_eq!(
        ancestry.root_of(&child),
        Some(A.to_owned()),
        "asked again once the meta line is whole: the child joins its parent's run"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_watch_that_finds_a_rollout_mid_meta_line_keeps_its_sub_agent_identity() {
    let (dir, child, content) = codex_with_partial_child("partial-meta");
    age(&child, Duration::ZERO); // a live rollout: its converter is kept
    let mut watching = options(Source::Codex, &dir, None);
    watching.watch = true;
    watching.interval = Duration::from_millis(10);
    let mut sink = Memory::new();
    let calls = std::cell::Cell::new(0);
    // After the first pass, Codex finishes the meta line and the turn.
    let stop = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            std::fs::write(&child, &content).expect("whole");
            age(&child, Duration::ZERO);
        }
        calls.get() >= 6
    };
    run(&watching, &mut sink, &mut std::io::sink(), &stop).expect("watches");
    let child_spans: Vec<&Span> = sink
        .sent
        .iter()
        .filter(|span| {
            attr(span, "gen_ai.agent.name") == &json!("reviewer")
                || attr(span, "gen_ai.request.model") == &json!("gpt-5-codex-mini")
        })
        .collect();
    assert!(!child_spans.is_empty(), "the child's turn was imported");
    for span in &sink.sent {
        assert_eq!(
            attr(span, "session.id"),
            &json!(A),
            "{}: every span joins the root's run, the sub-agent's included",
            span.name
        );
    }
    assert!(
        sink.sent
            .iter()
            .any(|span| operation(span) == Some("invoke_agent")),
        "the sub-agent is an agent span, not a session of its own"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sub-agent transcript copied under a scratch directory WITHOUT its meta
/// file. Returns the directory, the transcript, where the meta file goes and
/// where its content is.
fn subagent_without_meta(label: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = scratch(label);
    let subagents = dir.join("-tmp-demo-shop").join(S1).join("subagents");
    std::fs::create_dir_all(&subagents).expect("dir");
    let transcript = subagents.join("agent-a1b2c3.jsonl");
    std::fs::copy(cc_agent(), &transcript).expect("copy");
    let meta = transcript.with_extension("meta.json");
    (
        dir,
        transcript,
        meta,
        cc_agent().with_extension("meta.json"),
    )
}

/// The versions of the sub-agent's agent span `sink` was sent, in order.
fn agent_versions(sink: &Memory) -> Vec<&Span> {
    let agent_id = span_id(Source::ClaudeCode, S1, "agent", "a1b2c3");
    sink.sent
        .iter()
        .filter(|span| span.span_id == agent_id)
        .collect()
}

/// Asserts `span` is the sub-agent's agent span placed by its meta file.
fn assert_placed(span: &Span) {
    assert_eq!(
        span.parent_span_id.as_deref(),
        Some(span_id(Source::ClaudeCode, S1, "tool", "toolu_04").as_str()),
        "under the Agent tool call that spawned it"
    );
    assert_eq!(attr(span, "gen_ai.agent.name"), &json!("general-purpose"));
}

#[test]
fn a_subagent_meta_file_that_appears_after_a_restart_still_places_the_agent() {
    let (dir, _, meta, meta_source) = subagent_without_meta("late-meta-restart");
    let options = options(Source::ClaudeCode, &dir, Some(dir.join("state.json")));
    let mut sink = Memory::new();
    import(&options, &mut sink);
    assert_eq!(agent_versions(&sink)[0].parent_span_id, None, "no meta yet");

    // The importer is restarted; meanwhile Claude Code wrote the meta file.
    // The transcript itself did not change.
    std::fs::copy(&meta_source, &meta).expect("meta");
    let mut sink = Memory::new();
    let again = import(&options, &mut sink);
    let versions = agent_versions(&sink);
    assert_eq!(versions.len(), 1, "sent again: {again:?}");
    assert_placed(versions[0]);
    assert!(
        sink.sent
            .iter()
            .filter(|span| operation(span) == Some("chat"))
            .all(|span| attr(span, "gen_ai.agent.name") == &json!("general-purpose")),
        "the agent's calls are renamed too"
    );

    // Nothing changed since: nothing is sent.
    let mut sink = Memory::new();
    let idle = import(&options, &mut sink);
    assert_eq!((idle.spans_sent, idle.files_unchanged), (0, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_subagent_meta_file_that_appears_after_its_converter_was_dropped_still_places_the_agent() {
    // A watch keeps no converter for a transcript nobody wrote to lately;
    // the meta file appearing must still reach the spans.
    let (dir, transcript, meta, meta_source) = subagent_without_meta("late-meta-idle");
    age(&transcript, LIVE_IDLE + Duration::from_secs(60));
    let mut watching = options(Source::ClaudeCode, &dir, None);
    watching.watch = true;
    watching.interval = Duration::from_millis(10);
    let mut sink = Memory::new();
    let calls = std::cell::Cell::new(0);
    let stop = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            std::fs::copy(&meta_source, &meta).expect("meta");
        }
        calls.get() >= 6
    };
    let stats = run(&watching, &mut sink, &mut std::io::sink(), &stop).expect("watches");
    assert_eq!(stats.peak_live_converters, 0, "the transcript is idle");
    let versions = agent_versions(&sink);
    assert!(versions.len() >= 2, "sent, then sent again once placed");
    assert_eq!(versions[0].parent_span_id, None);
    assert_placed(versions[versions.len() - 1]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_transcript_rewritten_to_the_same_length_is_read_again() {
    let dir = scratch("same-length");
    copy_tree(&fixtures().join("claude-code"), &dir);
    let options = options(Source::ClaudeCode, &dir, Some(dir.join("state.json")));
    import(&options, &mut Memory::new());

    // A change past the head the state file digests, the size unchanged.
    let main = dir.join("-tmp-demo-shop").join(format!("{S1}.jsonl"));
    let original = std::fs::read_to_string(&main).expect("read");
    let rewritten = original.replacen(
        "Switched total to Decimal.",
        "Switched total to Integer.",
        1,
    );
    assert_eq!(rewritten.len(), original.len());
    assert_ne!(rewritten, original);
    std::fs::write(&main, &rewritten).expect("rewrite");
    age(&main, Duration::from_secs(30));
    let mut sink = Memory::new();
    let again = import(&options, &mut sink);
    assert_eq!(
        again.files_unchanged, 2,
        "the other two files only: {again:?}"
    );
    assert!(
        sink.sent
            .iter()
            .any(|span| attr(span, "gen_ai.output.messages")
                .to_string()
                .contains("Switched total to Integer.")),
        "the rewritten content was read and sent"
    );

    // A change inside the head, likewise.
    let other = dir.join("-tmp-demo-shop").join(format!("{S2}.jsonl"));
    let original = std::fs::read_to_string(&other).expect("read");
    let rewritten = original.replacen("flaky", "shaky", 1);
    assert_eq!(rewritten.len(), original.len());
    std::fs::write(&other, &rewritten).expect("rewrite");
    age(&other, Duration::from_secs(30));
    let mut sink = Memory::new();
    let again = import(&options, &mut sink);
    assert_eq!(again.files_unchanged, 2, "{again:?}");
    assert!(again.spans_sent > 0);

    // Nothing moved since: an idle tree costs a stat per file.
    let idle = import(&options, &mut Memory::new());
    assert_eq!((idle.spans_sent, idle.files_unchanged), (0, 3));

    // A state file written before stamps were: the head vouches for each
    // file once, its stamp is recorded, and nothing is sent again.
    let state_path = dir.join("state.json");
    let text = std::fs::read_to_string(&state_path).expect("state");
    let mut saved: Value = serde_json::from_str(&text).expect("json");
    for file in saved["destinations"]["test"]["files"]
        .as_object_mut()
        .expect("files")
        .values_mut()
    {
        file.as_object_mut().expect("file").remove("stamp");
    }
    std::fs::write(&state_path, saved.to_string()).expect("older state");
    let upgraded = import(&options, &mut Memory::new());
    assert_eq!((upgraded.spans_sent, upgraded.files_unchanged), (0, 3));
    let reloaded = state::State::load(&state_path).expect("state");
    assert!(discover(Source::ClaudeCode, &dir)
        .iter()
        .all(|file| reloaded
            .file("test", &file.path.to_string_lossy())
            .is_some_and(|saved| saved.stamp.is_some())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_oversized_subagent_meta_file_is_refused_and_counted() {
    let (dir, _, meta, _) = subagent_without_meta("meta-oversized");
    let padding = "x".repeat(claude_code::MAX_META_BYTES);
    std::fs::write(
        &meta,
        json!({"agentType": "general-purpose", "toolUseId": "toolu_04", "description": padding})
            .to_string(),
    )
    .expect("meta");
    let mut sink = Memory::new();
    let stats = import(&options(Source::ClaudeCode, &dir, None), &mut sink);
    assert_eq!(stats.files_failed, 1, "{:?}", stats.errors);
    assert!(
        stats
            .errors
            .iter()
            .any(|error| error.contains("agent-a1b2c3.meta.json") && error.contains("bytes")),
        "{:?}",
        stats.errors
    );
    assert_eq!(stats.files_read, 1, "the transcript itself still lands");
    assert_eq!(
        agent_versions(&sink)[0].parent_span_id,
        None,
        "unplaced: what the meta file says was not read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_beside_and_behind_a_transcript_are_read_within_a_cap() {
    let over = vec![b'x'; 4096];
    let error = read_capped(&over[..], 1024).expect_err("past the cap");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        read_capped(&over[..], 4096).expect("at the cap").len(),
        4096
    );
    // The cap holds while reading: an endless source (a file that keeps
    // growing) is refused once it passes the cap, not read to its end.
    let error = read_capped(std::io::repeat(b'x'), 1024).expect_err("endless");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    // A compressed rollout past its cap is refused before it is
    // decompressed.
    let dir = scratch("zst-cap");
    let path = dir.join(format!("rollout-x-{A}.jsonl.zst"));
    std::fs::write(&path, raw_zstd(&vec![b'a'; 4096])).expect("zst");
    let error = decompressed_within(&path, 1024).expect_err("past the cap");
    assert!(error.to_string().contains("1024"), "{error}");
    assert_eq!(
        decompressed_within(&path, 1 << 20).expect("within").len(),
        4096
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One Claude Code record of session `S2` at second `second`.
fn cc_record(uuid: &str, second: u32, body: Value) -> String {
    let mut record = json!({
        "parentUuid": null, "isSidechain": false, "cwd": "/tmp/demo-shop",
        "sessionId": S2, "version": "2.1.0", "uuid": uuid,
        "timestamp": format!("2026-01-05T12:00:{second:02}.000Z"),
    });
    for (key, value) in body.as_object().expect("object") {
        record[key] = value.clone();
    }
    record.to_string()
}

/// An assistant record asking for one `Edit` adding `line` to `file`.
fn cc_edit(uuid: &str, second: u32, call: &str, file: &str, line: &str) -> String {
    cc_record(
        uuid,
        second,
        json!({"type": "assistant", "message": {
            "id": format!("msg_{call}"), "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": call, "name": "Edit",
                "input": {"file_path": file, "old_string": "fn stale() {}", "new_string": line}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    )
}

/// The user record carrying `call`'s result.
fn cc_result(uuid: &str, second: u32, call: &str, text: &str, is_error: bool) -> String {
    cc_record(
        uuid,
        second,
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": call, "content": text, "is_error": is_error}]}}),
    )
}

#[test]
fn a_failed_claude_code_edit_is_not_blamed_for_the_line_a_later_edit_wrote() {
    let dir = scratch("failed-edit");
    let path = dir.join(format!("{S2}.jsonl"));
    let line = "let total = items.len();";
    let lines = [
        cc_record(
            "u1",
            0,
            json!({"type": "user", "message": {"role": "user", "content": "Count the items"}}),
        ),
        cc_edit("a1", 1, "toolu_fail", "/tmp/demo-shop/src/a.rs", line),
        cc_result(
            "r1",
            2,
            "toolu_fail",
            "<tool_use_error>String to replace not found in file.</tool_use_error>",
            true,
        ),
        cc_edit("a2", 3, "toolu_ok", "/tmp/demo-shop/src/a.rs", line),
        cc_result("r2", 4, "toolu_ok", "The file has been updated.", false),
        // A last call whose result was never written (the session stopped).
        cc_edit("a3", 5, "toolu_open", "/tmp/demo-shop/src/b.rs", line),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").expect("writes");
    let (spans, _) = claude(&path);
    let status = |call: &str| tool(&spans, S2, call).status;
    assert_eq!(status("toolu_fail"), "error");
    assert_eq!(status("toolu_ok"), "ok");
    assert_eq!(
        status("toolu_open"),
        "",
        "no result, no outcome: the status stays unset rather than claiming success"
    );

    let fixture = crate::tape::tests::TempStore::open("failed-edit", crate::Config::default());
    fixture.store().ingest_batch(spans).expect("ingests");
    let scope = crate::blame::BlameScope::new("src/a.rs", &[]).expect("scope");
    let blame = fixture
        .store()
        .blame(Some(""), &scope, line, 5)
        .expect("blames");
    let found: Vec<&str> = blame.matches.iter().map(|m| m.span_id.as_str()).collect();
    assert_eq!(
        found,
        [span_id(Source::ClaudeCode, S2, "tool", "toolu_ok").as_str()],
        "the failed attempt is not an introduction"
    );
    assert_eq!(blame.matches[0].outcome, crate::blame::EditOutcome::Applied);
    let open = fixture
        .store()
        .blame(
            Some(""),
            &crate::blame::BlameScope::new("src/b.rs", &[]).expect("scope"),
            line,
            5,
        )
        .expect("blames");
    assert_eq!(
        open.matches[0].outcome,
        crate::blame::EditOutcome::Unconfirmed,
        "a call whose result was never written is an attempt, not a confirmed introduction"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
