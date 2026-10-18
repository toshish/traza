//! Codex CLI rollouts to spans. The observed format is documented on the
//! parent module; this file is the state machine that reads it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::{
    blank_span, bounded, bounded_value, extend_end, preview, span_id, str_field, timestamp_ns,
    trace_id, Base, Converter, FileStats, Found, RecentDigests, Source, Spans, MAX_FIELD_BYTES,
    MAX_PART_BYTES, TURN_SPAN_NAME,
};
use crate::hash::hash128;
use crate::Event;

const SOURCE: Source = Source::Codex;

/// Input messages held for the next model response, at most.
const MAX_DELTA_MESSAGES: usize = 256;

/// Parts kept in one response's output message.
const MAX_OUTPUT_PARTS: usize = 1024;

/// Parent links followed to find a thread's root.
const MAX_ANCESTRY_DEPTH: usize = 16;

/// Which thread spawned which, learned from every rollout's first line, so a
/// sub-agent thread's spans join its root thread's run.
#[derive(Debug, Default)]
pub(crate) struct Ancestry {
    parents: HashMap<String, Option<String>>,
    /// Each rollout's own thread, once its meta line was read.
    threads: HashMap<PathBuf, String>,
    seen: HashSet<PathBuf>,
}

impl Ancestry {
    /// Reads the meta line of every file not settled before. A file whose
    /// first line is still being written (or could not be read) is asked
    /// again on the next call; one whose whole first line names no thread is
    /// settled without one.
    pub(crate) fn learn(&mut self, found: &[Found]) {
        for file in found {
            if self.seen.contains(&file.path) {
                continue;
            }
            match meta_of(&file.path) {
                MetaRead::Found(meta) => {
                    self.threads.insert(file.path.clone(), meta.thread.clone());
                    self.parents.insert(meta.thread, meta.parent);
                }
                MetaRead::Absent => {}
                MetaRead::Pending | MetaRead::Unreadable(_) => continue,
            }
            self.seen.insert(file.path.clone());
        }
    }

    /// The root thread of the rollout at `path`, when its meta line was read.
    pub(crate) fn root_of(&self, path: &Path) -> Option<String> {
        let thread = self.threads.get(path)?;
        let parent = self.parents.get(thread).cloned().flatten();
        Some(self.root(thread, parent.as_deref()))
    }

    /// The root of `thread`'s spawn tree, as far as the rollouts on disk tell.
    pub(crate) fn root(&self, thread: &str, parent: Option<&str>) -> String {
        let mut current = thread.to_owned();
        let mut next = parent.map(str::to_owned);
        for _ in 0..MAX_ANCESTRY_DEPTH {
            let Some(up) = next else {
                break;
            };
            current = up;
            next = self.parents.get(&current).cloned().flatten();
        }
        current
    }
}

/// A rollout's identity, from its meta line: its thread, and the thread that
/// spawned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    thread: String,
    parent: Option<String>,
}

/// What a rollout's first line says of its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MetaRead {
    /// Its thread and parent.
    Found(Meta),
    /// The first line is whole (or longer than a head read) and names no
    /// thread: the thread is the file name's, and there is no parent.
    Absent,
    /// The first line is still being written: nothing can be converted yet,
    /// and the identity everything would be derived from is not known.
    Pending,
    /// The file could not be read.
    Unreadable(String),
}

/// A rollout's identity, from its meta line.
pub(crate) fn meta_of(path: &Path) -> MetaRead {
    let line = match super::first_line(path) {
        Ok(super::FirstLine::Whole(line)) => line,
        Ok(super::FirstLine::Partial) => return MetaRead::Pending,
        Ok(super::FirstLine::TooLong) => return MetaRead::Absent,
        Err(error) => return MetaRead::Unreadable(error.to_string()),
    };
    let Ok(record) = serde_json::from_slice::<Value>(&line) else {
        return MetaRead::Absent;
    };
    let meta = match str_field(&record, "type") {
        Some("session_meta") => record.get("payload").cloned().unwrap_or(Value::Null),
        // The oldest builds wrote a bare `{id, timestamp, instructions}`.
        None if record.get("id").is_some() => record,
        _ => return MetaRead::Absent,
    };
    match str_field(&meta, "id") {
        Some(thread) => MetaRead::Found(Meta {
            thread: thread.to_owned(),
            parent: parent_of(&meta),
        }),
        None => MetaRead::Absent,
    }
}

/// Why a rollout has no converter yet.
#[derive(Debug)]
pub(crate) enum NotYet {
    /// Its meta line is still being written: ask again next poll.
    Pending,
    /// It could not be read.
    Unreadable(String),
}

/// The parent thread a `session_meta` payload names, if any.
fn parent_of(meta: &Value) -> Option<String> {
    str_field(meta, "parent_thread_id")
        .or_else(|| {
            meta.pointer("/source/subagent/thread_spawn/parent_thread_id")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Whether any string in `value` holds a Codex patch (depth-bounded).
fn carries_patch(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::String(text) => text.contains("*** Begin Patch"),
        Value::Array(items) => items.iter().any(|item| carries_patch(item, depth + 1)),
        Value::Object(map) => map.values().any(|item| carries_patch(item, depth + 1)),
        _ => false,
    }
}

/// Response-item payload types that are model output.
fn is_output(kind: &str) -> bool {
    matches!(
        kind,
        "reasoning"
            | "function_call"
            | "custom_tool_call"
            | "local_shell_call"
            | "web_search_call"
            | "tool_search_call"
            | "image_generation_call"
    )
}

/// Converts one Codex rollout file.
pub(crate) struct Codex {
    tenant: String,
    spans: Spans,
    base: Base,
    trace: String,
    thread: String,
    child: Option<ChildInfo>,
    agent_name: String,
    agent_span: Option<String>,
    provider: String,
    model: Option<String>,
    cwd: Option<String>,
    turn: Option<Turn>,
    goal_set: bool,
    /// Whether the rollout delimits turns with `task_started` events.
    turn_events: bool,
    response: Option<String>,
    /// The current turn's latest response, open or not: a tool item recorded
    /// after its response's token count still belongs to that response.
    last_response: Option<String>,
    /// Output response items seen in the current turn.
    turn_outputs: usize,
    /// The current turn's `item_completed` items and orphaned token counts,
    /// held until the turn ends (see [`Codex::settle`]). Bounded:
    /// [`MAX_PENDING`] entries, [`Codex::pending_budget`] bytes.
    pending: Vec<Pending>,
    /// Line bytes charged to `pending`.
    pending_bytes: usize,
    /// The most line bytes `pending` may be charged ([`MAX_PENDING_BYTES`]).
    pending_budget: usize,
    /// The line an item being converted came from: [`Codex::line`] normally,
    /// the held item's own line when a turn settles.
    item_line: u64,
    /// Between `task_started` and `task_complete`.
    in_turn: bool,
    /// Turns already settled (digests of their ids): an `item_completed`
    /// naming one arrived late and duplicates what was converted. Bounded:
    /// [`MAX_CLOSED_TURNS`].
    closed_turns: RecentDigests,
    /// Bytes of the line being fed.
    line_bytes: usize,
    awaiting_usage: Vec<String>,
    last_total: Option<String>,
    delta: Vec<Value>,
    /// Whether the next model call starts its conversation over — the first
    /// after a compaction — so its input is its whole context, not a delta.
    restart: bool,
    calls: HashMap<String, String>,
    last_input_at: Option<u64>,
    /// The latest timestamp seen: the oldest builds wrote item lines with
    /// none, and those are placed at it.
    last_at: Option<u64>,
    line: u64,
}

#[derive(Clone, Debug)]
struct ChildInfo {
    parent: String,
}

/// What a turn holds until it ends.
#[derive(Clone, Debug)]
enum Pending {
    /// An `event_msg/item_completed` item.
    Item { item: Value, at: u64, line: u64 },
    /// A token count no response was waiting for.
    Usage { usage: Value, at: u64, line: u64 },
}

/// Items held for one turn, at most; past it they are counted and dropped.
const MAX_PENDING: usize = 10_000;

/// Bytes of transcript lines the items held for one turn may have come
/// from, in all (each item is charged its whole line, an upper bound on what
/// it holds); past it they are counted and dropped like items past
/// [`MAX_PENDING`].
const MAX_PENDING_BYTES: usize = 256 * 1024 * 1024;

/// Settled turns remembered, by the digest of their id, to recognise an item
/// that arrives late: the most recent 4,096 (under 256 KiB). A late item
/// follows its turn's completion closely; an item naming a turn settled
/// longer ago than that is held as the current turn's (`closed_turns/
/// forgotten` counts the turns forgotten).
pub(crate) const MAX_CLOSED_TURNS: usize = 4096;

#[derive(Clone, Debug)]
struct Turn {
    span: String,
    prompted: bool,
}

impl Codex {
    /// A converter for the rollout at `path`, stamping `tenant`, with the
    /// identity its meta line gives; `ancestry` resolves a sub-agent
    /// thread's run. Every id the converter derives depends on that
    /// identity, so there is no converter until it is known: a rollout whose
    /// meta line is still being written is [`NotYet::Pending`].
    pub(crate) fn open(path: &Path, tenant: &str, ancestry: &Ancestry) -> Result<Self, NotYet> {
        match meta_of(path) {
            MetaRead::Found(meta) => Ok(Self::new(path, tenant, ancestry, Some(meta))),
            MetaRead::Absent => Ok(Self::new(path, tenant, ancestry, None)),
            MetaRead::Pending => Err(NotYet::Pending),
            MetaRead::Unreadable(error) => Err(NotYet::Unreadable(error)),
        }
    }

    /// A converter for the rollout at `path` whose identity is `meta` (`None`:
    /// a rollout that has none, whose thread is guessed from its file name).
    pub(crate) fn new(path: &Path, tenant: &str, ancestry: &Ancestry, meta: Option<Meta>) -> Self {
        let fallback = || {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.trim_end_matches(".zst").trim_end_matches(".jsonl"))
                .map(|stem| stem.rsplit('-').take(5).collect::<Vec<_>>())
                .map(|mut parts| {
                    parts.reverse();
                    parts.join("-")
                })
                .unwrap_or_else(|| "thread".to_owned())
        };
        let (thread, parent) = match meta {
            Some(meta) => (meta.thread, meta.parent),
            None => (fallback(), None),
        };
        let root = ancestry.root(&thread, parent.as_deref());
        let child = parent.map(|parent| ChildInfo { parent });
        Self {
            tenant: tenant.to_owned(),
            spans: Spans::default(),
            trace: trace_id(SOURCE, &root),
            base: Base {
                session: root,
                ..Base::default()
            },
            thread,
            agent_name: SOURCE.as_str().to_owned(),
            child,
            agent_span: None,
            provider: "openai".to_owned(),
            model: None,
            cwd: None,
            turn: None,
            goal_set: false,
            turn_events: false,
            response: None,
            last_response: None,
            turn_outputs: 0,
            pending: Vec::new(),
            pending_bytes: 0,
            pending_budget: MAX_PENDING_BYTES,
            item_line: 0,
            in_turn: false,
            closed_turns: RecentDigests::new(MAX_CLOSED_TURNS),
            line_bytes: 0,
            awaiting_usage: Vec::new(),
            last_total: None,
            delta: Vec::new(),
            restart: false,
            calls: HashMap::new(),
            last_input_at: None,
            last_at: None,
            line: 0,
        }
    }

    fn run_id(&self) -> &str {
        &self.base.session
    }

    /// Settled turns remembered.
    #[cfg(test)]
    pub(crate) fn closed_turns_len(&self) -> usize {
        self.closed_turns.len()
    }

    /// Serialized bytes of what the current turn holds.
    #[cfg(test)]
    pub(crate) fn pending_bytes(&self) -> usize {
        self.pending
            .iter()
            .map(|held| match held {
                Pending::Item { item, .. } => item.to_string().len(),
                Pending::Usage { usage, .. } => usage.to_string().len(),
            })
            .sum()
    }

    /// This converter with `bytes` as its held-items budget.
    #[cfg(test)]
    pub(crate) fn with_pending_budget(mut self, bytes: usize) -> Self {
        self.pending_budget = bytes;
        self
    }

    fn key(&self, item: &str) -> String {
        format!("{}:{item}", self.thread)
    }

    fn session_meta(&mut self, meta: &Value) {
        if let Some(cwd) = str_field(meta, "cwd") {
            self.base.project = Some(cwd.to_owned());
            self.cwd = Some(cwd.to_owned());
        }
        if let Some(version) = str_field(meta, "cli_version") {
            self.base.version = Some(version.to_owned());
        }
        if let Some(branch) = meta.pointer("/git/branch").and_then(Value::as_str) {
            self.base.branch = Some(branch.to_owned());
        }
        if let Some(provider) = str_field(meta, "model_provider") {
            self.provider = provider.to_owned();
        }
        if self.child.is_some() {
            let name = str_field(meta, "agent_role")
                .or_else(|| str_field(meta, "agent_nickname"))
                .or_else(|| {
                    meta.pointer("/source/subagent/thread_spawn/agent_role")
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    meta.pointer("/source/subagent/thread_spawn/agent_nickname")
                        .and_then(Value::as_str)
                })
                .unwrap_or("subagent");
            self.agent_name = name.to_owned();
        }
    }

    /// The span responses hang under: the current turn (opening an implicit
    /// one for a rollout without turn events) or the sub-agent's span.
    fn container(&mut self, at: u64, end: u64) -> Option<String> {
        if self.child.is_some() {
            return self.agent(at, end);
        }
        if self.turn.is_none() {
            let key = self.key(&format!("implicit:{}", self.line));
            self.open_turn(&key, at, end);
        }
        self.turn.as_ref().map(|turn| turn.span.clone())
    }

    fn agent(&mut self, at: u64, end: u64) -> Option<String> {
        if let Some(id) = &self.agent_span {
            let id = id.clone();
            if let Some(span) = self.spans.get_mut(&id, end) {
                extend_end(span, at);
            }
            return Some(id);
        }
        let parent_thread = self.child.as_ref()?.parent.clone();
        let id = span_id(SOURCE, self.run_id(), "agent", &self.thread);
        let (tenant, trace, base, name, thread) = (
            self.tenant.clone(),
            self.trace.clone(),
            self.base.clone(),
            self.agent_name.clone(),
            self.thread.clone(),
        );
        self.spans.touch(&id, end, || {
            let mut span = blank_span(
                SOURCE,
                &tenant,
                &trace,
                &id,
                None,
                format!("invoke_agent {name}"),
                at,
            );
            base.apply(SOURCE, &mut span.attributes);
            let a = &mut span.attributes;
            a.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
            a.insert("gen_ai.agent.name".into(), json!(name));
            a.insert("gen_ai.agent.id".into(), json!(thread));
            a.insert("codex.parent_thread_id".into(), json!(parent_thread));
            span
        })?;
        self.agent_span = Some(id.clone());
        Some(id)
    }

    fn open_turn(&mut self, key: &str, at: u64, end: u64) {
        let id = span_id(SOURCE, self.run_id(), "turn", key);
        let (tenant, trace, base) = (self.tenant.clone(), self.trace.clone(), self.base.clone());
        if self
            .spans
            .touch(&id, end, || {
                let mut span = blank_span(
                    SOURCE,
                    &tenant,
                    &trace,
                    &id,
                    None,
                    TURN_SPAN_NAME.to_owned(),
                    at,
                );
                base.apply(SOURCE, &mut span.attributes);
                span
            })
            .is_some()
        {
            self.turn = Some(Turn {
                span: id,
                prompted: false,
            });
        }
    }

    fn current_container(&self) -> Option<String> {
        if self.child.is_some() {
            self.agent_span.clone()
        } else {
            self.turn.as_ref().map(|turn| turn.span.clone())
        }
    }

    fn extend_container(&mut self, at: u64, end: u64) {
        if let Some(id) = self.current_container() {
            if let Some(span) = self.spans.get_mut(&id, end) {
                extend_end(span, at);
            }
        }
    }

    fn push_delta(&mut self, message: Value, stats: &mut FileStats) {
        if self.delta.len() >= MAX_DELTA_MESSAGES {
            self.delta.remove(0);
            stats.record("delta-overflow");
        }
        self.delta.push(message);
    }

    /// The open response, created when the first output item arrives.
    fn response(&mut self, at: u64, end: u64) -> Option<String> {
        if let Some(id) = &self.response {
            return Some(id.clone());
        }
        let container = self.container(at, end)?;
        let id = span_id(
            SOURCE,
            self.run_id(),
            "llm",
            &self.key(&self.item_line.to_string()),
        );
        let started = self.last_input_at.filter(|t| *t <= at).unwrap_or(at);
        let input = std::mem::take(&mut self.delta);
        // A delta of its thread's conversation, unless it starts it over.
        let delta = !std::mem::take(&mut self.restart);
        let (tenant, trace, base, agent, provider, model, thread) = (
            self.tenant.clone(),
            self.trace.clone(),
            self.base.clone(),
            self.agent_name.clone(),
            self.provider.clone(),
            self.model.clone(),
            self.thread.clone(),
        );
        self.spans.touch(&id, end, || {
            let name = format!("chat {}", model.as_deref().unwrap_or("unknown"));
            let mut span = blank_span(SOURCE, &tenant, &trace, &id, Some(container), name, started);
            base.apply(SOURCE, &mut span.attributes);
            let a = &mut span.attributes;
            a.insert("gen_ai.operation.name".into(), json!("chat"));
            a.insert("gen_ai.provider.name".into(), json!(provider));
            if let Some(model) = &model {
                a.insert("gen_ai.request.model".into(), json!(model));
            }
            a.insert("gen_ai.agent.name".into(), json!(agent));
            a.insert(crate::xray::CONVERSATION_ATTRIBUTE.into(), json!(thread));
            a.insert(crate::xray::INPUT_DELTA_ATTRIBUTE.into(), json!(delta));
            if !input.is_empty() {
                a.insert("gen_ai.input.messages".into(), Value::Array(input));
            }
            a.insert(
                "gen_ai.output.messages".into(),
                json!([{"role": "assistant", "parts": []}]),
            );
            span
        })?;
        self.awaiting_usage.push(id.clone());
        self.response = Some(id.clone());
        self.last_response = Some(id.clone());
        Some(id)
    }

    fn add_part(&mut self, part: Value, finish: Option<&str>, at: u64, end: u64) {
        let Some(id) = self.response(at, end) else {
            return;
        };
        if let Some(span) = self.spans.get_mut(&id, end) {
            extend_end(span, at);
            if let Some(Value::Array(messages)) = span.attributes.get_mut("gen_ai.output.messages")
            {
                if let Some(Value::Object(output)) = messages.first_mut() {
                    if let Some(Value::Array(parts)) = output.get_mut("parts") {
                        if parts.len() < MAX_OUTPUT_PARTS {
                            parts.push(part);
                        }
                    }
                    if let Some(finish) = finish {
                        output.insert("finish_reason".into(), json!(finish));
                    }
                }
            }
        }
        self.extend_container(at, end);
    }

    fn close_response(&mut self) {
        self.response = None;
    }

    fn tool_call(
        &mut self,
        call_key: &str,
        name: &str,
        arguments: Value,
        at: u64,
        end: u64,
    ) -> Option<String> {
        let llm = self.response(at, end)?;
        self.tool_under(&llm, call_key, name, arguments, at, end)
    }

    /// A tool span under the llm span `llm`.
    fn tool_under(
        &mut self,
        llm: &str,
        call_key: &str,
        name: &str,
        arguments: Value,
        at: u64,
        end: u64,
    ) -> Option<String> {
        let llm = llm.to_owned();
        let id = span_id(SOURCE, self.run_id(), "tool", &self.key(call_key));
        let (tenant, trace, base, agent, cwd) = (
            self.tenant.clone(),
            self.trace.clone(),
            self.base.clone(),
            self.agent_name.clone(),
            self.cwd.clone(),
        );
        let call_id = call_key.to_owned();
        let tool_name = name.to_owned();
        let arguments = bounded_value(&arguments, MAX_FIELD_BYTES);
        // A patch applied through a general tool (an older build's
        // `shell ["apply_patch", …]`) is marked, so blame finds it by an
        // index lookup instead of reading every shell command.
        let patched = !crate::blame::EDIT_TOOLS.contains(&name) && carries_patch(&arguments, 0);
        let made = self.spans.touch(&id, end, || {
            let mut span = blank_span(
                SOURCE,
                &tenant,
                &trace,
                &id,
                Some(llm),
                format!("execute_tool {tool_name}"),
                at,
            );
            // No outcome until the output (or the item's own status) says
            // one: a call whose output was never written did not succeed as
            // far as anyone knows (blame reads it as unconfirmed).
            span.status = String::new();
            base.apply(SOURCE, &mut span.attributes);
            let a = &mut span.attributes;
            a.insert("gen_ai.operation.name".into(), json!("execute_tool"));
            a.insert("gen_ai.tool.name".into(), json!(tool_name));
            a.insert("gen_ai.tool.call.id".into(), json!(call_id));
            a.insert("gen_ai.tool.call.arguments".into(), arguments);
            a.insert("gen_ai.agent.name".into(), json!(agent));
            // Patches name files relative to the working directory; blame
            // joins them with this.
            if let Some(cwd) = cwd {
                a.insert("code.cwd".into(), json!(cwd));
            }
            if patched {
                a.insert(crate::blame::EDIT_MARKER.into(), json!(true));
            }
            span
        });
        made?;
        self.calls.insert(call_key.to_owned(), id.clone());
        Some(id)
    }

    fn tool_output(
        &mut self,
        call_id: &str,
        output: Option<&Value>,
        at: u64,
        end: u64,
        stats: &mut FileStats,
    ) {
        let (text, exit) = output_text(output);
        match self.calls.get(call_id).cloned() {
            Some(id) => {
                if let Some(span) = self.spans.get_mut(&id, end) {
                    span.attributes.insert(
                        "gen_ai.tool.call.result".into(),
                        json!(bounded(&text, MAX_FIELD_BYTES)),
                    );
                    if let Some(code) = exit {
                        span.attributes
                            .insert("process.exit.code".into(), json!(code));
                    }
                    span.status = if exit.is_some_and(|code| code != 0) {
                        "error"
                    } else {
                        "ok"
                    }
                    .to_owned();
                    extend_end(span, at);
                }
            }
            None => stats.record("tool_output/unmatched"),
        }
        self.push_delta(
            json!({"role": "tool", "parts": [{
                "type": "tool_call_response",
                "id": call_id,
                "response": bounded(&text, MAX_PART_BYTES),
            }]}),
            stats,
        );
    }

    fn input_seen(&mut self, at: u64, end: u64) {
        self.close_response();
        self.last_input_at = Some(at);
        self.extend_container(at, end);
    }

    fn response_item(&mut self, payload: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let kind = str_field(payload, "type").unwrap_or("<none>");
        stats.record(&format!("response_item/{kind}"));
        if is_output(kind) || (kind == "message" && str_field(payload, "role") == Some("assistant"))
        {
            self.turn_outputs += 1;
        }
        match kind {
            "message" => {
                let role = str_field(payload, "role").unwrap_or("user");
                let (texts, images) = message_texts(payload.get("content"));
                let text = texts.join("\n");
                if role == "assistant" {
                    let finish = str_field(payload, "phase")
                        .filter(|phase| *phase == "final_answer")
                        .map(|_| "stop");
                    self.add_part(
                        json!({"type": "text", "content": bounded(&text, MAX_PART_BYTES)}),
                        finish,
                        at,
                        end,
                    );
                    if self.child.is_some() {
                        if let Some(id) = self.agent_span.clone() {
                            if let Some(span) = self.spans.get_mut(&id, end) {
                                span.attributes.insert(
                                    "output.value".into(),
                                    json!(bounded(&text, MAX_FIELD_BYTES)),
                                );
                            }
                        }
                    }
                    return;
                }
                let mut parts =
                    vec![json!({"type": "text", "content": bounded(&text, MAX_PART_BYTES)})];
                for _ in 0..images {
                    parts.push(json!({"type": "image", "omitted": true}));
                }
                let wire_role = if role == "user" { "user" } else { "system" };
                if role == "user" && is_prompt(&text) {
                    self.prompt(&text, at, end);
                }
                self.input_seen(at, end);
                self.push_delta(json!({"role": wire_role, "parts": parts}), stats);
            }
            "agent_message" => {
                let (texts, _) = message_texts(payload.get("content"));
                self.input_seen(at, end);
                self.push_delta(
                    json!({"role": "user", "parts": [{
                        "type": "text",
                        "content": bounded(&texts.join("\n"), MAX_PART_BYTES),
                    }], "author": str_field(payload, "author")}),
                    stats,
                );
            }
            "reasoning" => {
                let summary: Vec<&str> = payload
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| str_field(item, "text"))
                            .collect()
                    })
                    .unwrap_or_default();
                let text = summary.join("\n");
                if text.is_empty() {
                    // Encrypted reasoning still marks a response in progress.
                    let _ = self.response(at, end);
                } else {
                    self.add_part(
                        json!({"type": "reasoning", "content": bounded(&text, MAX_PART_BYTES)}),
                        None,
                        at,
                        end,
                    );
                }
            }
            "function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call" => {
                let call_id = str_field(payload, "call_id")
                    .map(str::to_owned)
                    .or_else(|| str_field(payload, "id").map(str::to_owned))
                    .unwrap_or_else(|| format!("line{}", self.line));
                let name = match kind {
                    "local_shell_call" => "local_shell",
                    "tool_search_call" => "tool_search",
                    _ => str_field(payload, "name").unwrap_or("tool"),
                }
                .to_owned();
                let arguments = match kind {
                    "function_call" => parse_arguments(payload.get("arguments")),
                    "custom_tool_call" => payload.get("input").cloned().unwrap_or(Value::Null),
                    "local_shell_call" => payload.get("action").cloned().unwrap_or(Value::Null),
                    _ => payload.get("arguments").cloned().unwrap_or(Value::Null),
                };
                self.add_part(
                    json!({
                        "type": "tool_call",
                        "id": call_id,
                        "name": name,
                        "arguments": bounded_value(&arguments, MAX_PART_BYTES),
                    }),
                    Some("tool_call"),
                    at,
                    end,
                );
                self.tool_call(&call_id, &name, arguments, at, end);
            }
            "web_search_call" | "image_generation_call" => {
                let key = format!("line{}", self.line);
                let name = if kind == "web_search_call" {
                    "web_search"
                } else {
                    "image_generation"
                };
                let arguments = payload.get("action").cloned().unwrap_or_else(|| json!({}));
                self.add_part(
                    json!({"type": "tool_call", "id": key, "name": name,
                           "arguments": bounded_value(&arguments, MAX_PART_BYTES)}),
                    None,
                    at,
                    end,
                );
                if let Some(id) = self.tool_call(&key, name, arguments, at, end) {
                    let failed = str_field(payload, "status") == Some("failed");
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        span.status = if failed { "error" } else { "ok" }.to_owned();
                    }
                }
            }
            "function_call_output"
            | "custom_tool_call_output"
            | "local_shell_call_output"
            | "tool_search_output" => {
                let call_id = str_field(payload, "call_id").unwrap_or_default().to_owned();
                let output = if kind == "tool_search_output" {
                    let count = payload
                        .get("tools")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    Some(json!(format!("{count} tool groups found")))
                } else {
                    payload.get("output").cloned()
                };
                self.input_seen(at, end);
                self.tool_output(&call_id, output.as_ref(), at, end, stats);
            }
            "compaction" => {}
            _ => {
                stats.unknown_blocks += 1;
            }
        }
    }

    fn prompt(&mut self, text: &str, at: u64, end: u64) {
        if self.child.is_some() {
            if let Some(id) = self.agent(at, end) {
                if let Some(span) = self.spans.get_mut(&id, end) {
                    span.events.push(prompt_event(text, at));
                    span.attributes
                        .entry("input.value")
                        .or_insert_with(|| json!(bounded(text, MAX_FIELD_BYTES)));
                }
            }
            return;
        }
        // Rollouts with turn events open turns from them; the oldest builds
        // have none, so there every prompt opens the next turn.
        let open_new = match &self.turn {
            None => true,
            Some(turn) => turn.prompted && !self.turn_events,
        };
        if open_new {
            self.close_response();
            let key = self.key(&format!("prompt:{}", self.line));
            self.open_turn(&key, at, end);
        }
        let goal = (!self.goal_set).then(|| preview(text, 200));
        if let Some(turn) = self.turn.as_mut() {
            if turn.prompted {
                return;
            }
            turn.prompted = true;
            let id = turn.span.clone();
            if let Some(span) = self.spans.get_mut(&id, end) {
                span.events.push(prompt_event(text, at));
                if let Some(goal) = goal {
                    span.attributes.insert("session.goal".into(), json!(goal));
                    self.goal_set = true;
                }
            }
        }
    }

    fn event_msg(&mut self, payload: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let kind = str_field(payload, "type").unwrap_or("<none>");
        stats.record(&format!("event_msg/{kind}"));
        match kind {
            "task_started" => {
                // A turn that never completed settles now. What was held
                // BETWEEN turns (after a completion) joins this one.
                if self.in_turn {
                    self.settle(at, end, stats);
                }
                self.in_turn = true;
                self.turn_events = true;
                self.close_response();
                self.awaiting_usage.clear();
                if self.child.is_none() {
                    if let Some(turn_id) = str_field(payload, "turn_id") {
                        let key = self.key(turn_id);
                        self.open_turn(&key, at, end);
                    }
                } else {
                    let _ = self.agent(at, end);
                }
                self.last_input_at = Some(at);
            }
            "task_complete" | "turn_aborted" => {
                self.settle(at, end, stats);
                self.in_turn = false;
                if let Some(turn_id) = str_field(payload, "turn_id") {
                    self.closed_turns.insert(
                        hash128(turn_id.as_bytes()),
                        stats,
                        "closed_turns/forgotten",
                    );
                }
                self.close_response();
                let id = self.current_container();
                if let Some(id) = id {
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        extend_end(span, at);
                        if kind == "turn_aborted" {
                            span.attributes.insert(
                                "codex.turn.aborted".into(),
                                json!(str_field(payload, "reason").unwrap_or("aborted")),
                            );
                        } else if let Some(error) = payload.get("error").filter(|e| !e.is_null()) {
                            span.status = "error".to_owned();
                            let message = str_field(error, "message")
                                .map(str::to_owned)
                                .unwrap_or_else(|| error.to_string());
                            span.attributes
                                .insert("error.message".into(), json!(preview(&message, 500)));
                        } else {
                            span.status = "ok".to_owned();
                        }
                    }
                }
                if self.child.is_none() && kind == "task_complete" {
                    // The turn is over; later lines belong to the next one.
                    self.turn = None;
                }
            }
            "token_count" => self.token_count(payload, at, end, stats),
            "item_completed" => {
                let late = str_field(payload, "turn_id")
                    .is_some_and(|turn| self.closed_turns.contains(&hash128(turn.as_bytes())));
                if late {
                    stats.record("item_completed/late");
                } else if let Some(item) = payload.get("item") {
                    let held = Pending::Item {
                        item: item.clone(),
                        at,
                        line: self.line,
                    };
                    self.hold(held, stats, "item_completed/overflow");
                }
            }
            "user_message"
            | "agent_message"
            | "agent_reasoning"
            | "exec_command_begin"
            | "exec_command_end"
            | "patch_apply_begin"
            | "patch_apply_end"
            | "thread_settings_applied"
            | "thread_goal_updated"
            | "mcp_tool_call_begin"
            | "mcp_tool_call_end"
            | "web_search_begin"
            | "web_search_end" => {}
            _ => {}
        }
    }

    fn token_count(&mut self, payload: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let Some(info) = payload.get("info").filter(|info| !info.is_null()) else {
            return;
        };
        let total = info.get("total_token_usage").map(Value::to_string);
        if total.is_some() && total == self.last_total {
            return; // a repeat: nothing new was spent
        }
        self.last_total = total;
        let Some(usage) = info.get("last_token_usage") else {
            return;
        };
        // The most recent response still without usage: robust to a response
        // whose count never arrived (an aborted turn). A count no response
        // is waiting for is a call whose items were recorded some other way
        // (or not at all): held until the turn ends.
        let Some(id) = self.awaiting_usage.pop() else {
            let held = Pending::Usage {
                usage: usage.clone(),
                at,
                line: self.line,
            };
            self.hold(held, stats, "token_count/unattributed");
            return;
        };
        self.set_usage(&id, usage, end);
    }

    /// Holds `entry` for the current turn, charged the line it came from,
    /// unless that would pass [`MAX_PENDING`] entries or the byte budget:
    /// then it is counted under `overflow` and dropped.
    fn hold(&mut self, entry: Pending, stats: &mut FileStats, overflow: &str) {
        let bytes = self.pending_bytes.saturating_add(self.line_bytes);
        if self.pending.len() < MAX_PENDING && bytes <= self.pending_budget {
            self.pending.push(entry);
            self.pending_bytes = bytes;
        } else {
            stats.record(overflow);
        }
    }

    fn set_usage(&mut self, id: &str, usage: &Value, end: u64) {
        if let Some(span) = self.spans.get_mut(id, end) {
            let a = &mut span.attributes;
            for (from, to) in [
                ("input_tokens", "gen_ai.usage.input_tokens"),
                ("output_tokens", "gen_ai.usage.output_tokens"),
                (
                    "cached_input_tokens",
                    "gen_ai.usage.cache_read.input_tokens",
                ),
                (
                    "reasoning_output_tokens",
                    "gen_ai.usage.reasoning.output_tokens",
                ),
            ] {
                if let Some(count) = usage.get(from).and_then(Value::as_u64) {
                    a.insert(to.into(), json!(count));
                }
            }
            if let Some(count) = usage
                .get("cache_write_input_tokens")
                .and_then(Value::as_u64)
                .filter(|count| *count > 0)
            {
                a.insert(
                    "gen_ai.usage.cache_creation.input_tokens".into(),
                    json!(count),
                );
            }
        }
    }
}

impl Codex {
    /// Ends the current turn's held items. When the turn recorded its model
    /// output as response items, held items duplicate them and are dropped,
    /// and each held token count becomes a call whose content was not
    /// recorded. When it did not — newer Codex builds record some turns only
    /// as `item_completed` events — the held items and counts are replayed,
    /// in order, as that turn's responses and tool calls.
    fn settle(&mut self, at: u64, end: u64, stats: &mut FileStats) {
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let replay = self.turn_outputs == 0;
        self.turn_outputs = 0;
        if pending.is_empty() {
            self.last_response = None;
            return;
        }
        for entry in pending {
            match entry {
                Pending::Item { item, at, line } => {
                    if replay {
                        self.item_line = line;
                        self.replay_item(&item, at, end, stats);
                    } else {
                        stats.record("item_completed/duplicate");
                    }
                }
                Pending::Usage { usage, at, line } => {
                    self.item_line = line;
                    match self.awaiting_usage.pop() {
                        Some(id) if replay => self.set_usage(&id, &usage, end),
                        _ => {
                            stats.record("token_count/content-unrecorded");
                            self.close_response();
                            if let Some(id) = self.response(at, end) {
                                self.awaiting_usage.retain(|waiting| *waiting != id);
                                self.set_usage(&id, &usage, end);
                                if let Some(span) = self.spans.get_mut(&id, end) {
                                    span.attributes
                                        .insert("codex.content_unrecorded".into(), json!(true));
                                }
                            }
                        }
                    }
                    self.close_response();
                }
            }
        }
        self.item_line = self.line;
        self.last_response = None;
        let _ = at;
    }

    /// One held `item_completed` item, as part of its turn's replay.
    fn replay_item(&mut self, item: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let kind = str_field(item, "type").unwrap_or("<none>");
        stats.record(&format!("item_completed/{kind}"));
        let texts = |key: &str| -> String {
            item.get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|entry| entry.as_str().or_else(|| str_field(entry, "text")))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        match kind {
            "Reasoning" => {
                let text = texts("summary_text");
                if text.is_empty() {
                    let _ = self.response(at, end);
                } else {
                    self.add_part(
                        json!({"type": "reasoning", "content": bounded(&text, MAX_PART_BYTES)}),
                        None,
                        at,
                        end,
                    );
                }
            }
            "AgentMessage" => {
                let text = texts("content");
                let finish = (str_field(item, "phase") == Some("final_answer")).then_some("stop");
                self.add_part(
                    json!({"type": "text", "content": bounded(&text, MAX_PART_BYTES)}),
                    finish,
                    at,
                    end,
                );
            }
            "UserMessage" => {
                let text = texts("content");
                if is_prompt(&text) {
                    self.prompt(&text, at, end);
                }
            }
            "ContextCompaction" => {
                self.compacted(None, stats);
                if let Some(id) = self.current_container() {
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        span.events.push(Event {
                            name: "codex.compaction".into(),
                            timestamp_ns: at,
                            attributes: Map::new(),
                        });
                    }
                }
            }
            "CommandExecution" | "FileChange" | "McpToolCall" | "WebSearch" | "Extension"
            | "ImageView" | "DynamicToolCall" => self.replay_tool(kind, item, at, end, stats),
            _ => {}
        }
    }

    /// The thread's conversation was compacted: the next model call starts
    /// it over, from the replacement history when the record carries one
    /// (its messages become that call's input, its whole context as far as
    /// the rollout says), else from what follows.
    fn compacted(&mut self, payload: Option<&Value>, stats: &mut FileStats) {
        self.delta.clear();
        self.restart = true;
        let Some(history) = payload
            .and_then(|payload| payload.get("replacement_history"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for item in history {
            if str_field(item, "type") != Some("message") {
                continue;
            }
            let (texts, images) = message_texts(item.get("content"));
            let role = match str_field(item, "role") {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => "system",
            };
            let mut parts = vec![
                json!({"type": "text", "content": bounded(&texts.join("\n"), MAX_PART_BYTES)}),
            ];
            for _ in 0..images {
                parts.push(json!({"type": "image", "omitted": true}));
            }
            self.push_delta(json!({"role": role, "parts": parts}), stats);
        }
    }

    /// A tool item: the call and its result in one record. It belongs to the
    /// response that requested it — usually already closed by its token
    /// count — and its result is input to the next one.
    fn replay_tool(&mut self, kind: &str, item: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let pick = |keys: &[&str]| -> Value {
            let mut picked = Map::new();
            for key in keys {
                if let Some(value) = item.get(*key).filter(|value| !value.is_null()) {
                    picked.insert((*key).to_owned(), value.clone());
                }
            }
            Value::Object(picked)
        };
        let failed = str_field(item, "status") == Some("failed");
        let (name, arguments, result, exit) = match kind {
            "CommandExecution" => (
                "exec_command".to_owned(),
                pick(&["command", "cwd"]),
                str_field(item, "aggregated_output")
                    .or_else(|| str_field(item, "formatted_output"))
                    .unwrap_or_default()
                    .to_owned(),
                item.get("exit_code").and_then(Value::as_i64),
            ),
            "FileChange" => (
                "apply_patch".to_owned(),
                pick(&["changes"]),
                [str_field(item, "stdout"), str_field(item, "stderr")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n"),
                None,
            ),
            "McpToolCall" => {
                let content = item
                    .pointer("/result/content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| str_field(part, "text"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .or_else(|| {
                        item.pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                (
                    str_field(item, "tool").unwrap_or("mcp_tool").to_owned(),
                    item.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    content,
                    None,
                )
            }
            "ImageView" => (
                "view_image".to_owned(),
                pick(&["path"]),
                String::new(),
                None,
            ),
            "WebSearch" | "Extension" => (
                str_field(item, "kind").unwrap_or("web_search").to_owned(),
                pick(&["query", "action"]),
                item.get("results")
                    .and_then(Value::as_array)
                    .map_or(String::new(), |results| {
                        format!("{} results", results.len())
                    }),
                None,
            ),
            _ => ("dynamic_tool".to_owned(), item.clone(), String::new(), None),
        };
        let mcp_error = item.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
        let parent = match self.response.clone().or_else(|| self.last_response.clone()) {
            Some(parent) => Some(parent),
            None => self.response(at, end),
        };
        let Some(parent) = parent else {
            return;
        };
        let key = format!(
            "item:{}",
            str_field(item, "id").map_or_else(|| self.item_line.to_string(), str::to_owned)
        );
        let started = item
            .get("duration")
            .and_then(|duration| {
                let secs = duration.get("secs")?.as_u64()?;
                let nanos = duration.get("nanos")?.as_u64()?;
                Some(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
            })
            .map_or(at, |duration| at.saturating_sub(duration));
        if let Some(id) = self.tool_under(&parent, &key, &name, arguments, started, end) {
            if let Some(span) = self.spans.get_mut(&id, end) {
                span.attributes.insert(
                    "gen_ai.tool.call.result".into(),
                    json!(bounded(&result, MAX_FIELD_BYTES)),
                );
                if let Some(code) = exit {
                    span.attributes
                        .insert("process.exit.code".into(), json!(code));
                }
                if kind == "McpToolCall" {
                    if let Some(server) = str_field(item, "server") {
                        span.attributes
                            .insert("codex.mcp.server".into(), json!(server));
                    }
                }
                let error = failed || mcp_error || exit.is_some_and(|code| code != 0);
                span.status = if error { "error" } else { "ok" }.to_owned();
                extend_end(span, at);
            }
        }
        self.input_seen(at, end);
        self.push_delta(
            json!({"role": "tool", "parts": [{
                "type": "tool_call_response",
                "id": key,
                "response": bounded(&result, MAX_PART_BYTES),
            }]}),
            stats,
        );
    }
}

impl Converter for Codex {
    fn finish(&mut self, end: u64, stats: &mut FileStats) {
        let at = self.last_at.unwrap_or(0);
        self.settle(at, end, stats);
    }

    fn feed(&mut self, line: &[u8], end: u64, stats: &mut FileStats) {
        self.line += 1;
        self.item_line = self.line;
        self.line_bytes = line.len();
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            stats.malformed_lines += 1;
            return;
        };
        if !record.is_object() {
            stats.malformed_lines += 1;
            return;
        }
        let at = timestamp_ns(&record).or(self.last_at);
        self.last_at = at;
        let kind = str_field(&record, "type").map(str::to_owned);
        let payload = record.get("payload");
        match (kind.as_deref(), payload, at) {
            (Some("session_meta"), Some(meta), _) => {
                stats.record("session_meta");
                self.session_meta(meta);
                if let Some(at) = at {
                    if self.child.is_some() {
                        let _ = self.agent(at, end);
                    }
                }
            }
            (Some("turn_context"), Some(context), _) => {
                stats.record("turn_context");
                if let Some(model) = str_field(context, "model") {
                    self.model = Some(model.to_owned());
                }
                if let Some(cwd) = str_field(context, "cwd") {
                    self.cwd = Some(cwd.to_owned());
                }
                if let (Some(turn_id), Some(at), None) =
                    (str_field(context, "turn_id"), at, self.child.as_ref())
                {
                    let key = self.key(turn_id);
                    let id = span_id(SOURCE, self.run_id(), "turn", &key);
                    if self.turn.as_ref().map(|turn| turn.span.as_str()) != Some(id.as_str()) {
                        self.settle(at, end, stats);
                        self.close_response();
                        self.open_turn(&key, at, end);
                    }
                }
            }
            (Some("response_item"), Some(item), Some(at)) => {
                self.response_item(item, at, end, stats)
            }
            (Some("event_msg"), Some(event), Some(at)) => self.event_msg(event, at, end, stats),
            (Some("compacted"), _, Some(at)) => {
                stats.record("compacted");
                self.close_response();
                self.compacted(record.get("payload"), stats);
                if let Some(id) = self.current_container() {
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        span.events.push(Event {
                            name: "codex.compaction".into(),
                            timestamp_ns: at,
                            attributes: Map::new(),
                        });
                        extend_end(span, at);
                    }
                }
            }
            (
                Some(
                    "token_usage_record"
                    | "world_state"
                    | "inter_agent_communication_metadata"
                    | "realtime_item",
                ),
                _,
                _,
            ) => stats.record(kind.as_deref().unwrap_or_default()),
            // The oldest builds: response items as bare lines, and a first
            // line of `{id, timestamp, instructions}`.
            (Some(item_kind), None, Some(at))
                if is_output(item_kind)
                    || matches!(
                        item_kind,
                        "message" | "function_call_output" | "local_shell_call_output"
                    ) =>
            {
                self.response_item(&record, at, end, stats)
            }
            (None, None, _) if record.get("record_type").is_some() => stats.record("state"),
            (None, None, _) if record.get("id").is_some() && self.line == 1 => {
                stats.record("session_meta");
                self.session_meta(&record);
            }
            (Some(other), _, _) => stats.unknown(other),
            (None, _, _) => stats.unknown("<none>"),
        }
    }

    fn spans(&mut self) -> &mut Spans {
        &mut self.spans
    }

    fn run(&self) -> Option<String> {
        Some(self.base.session.clone())
    }
}

/// Whether a user message's text is a human prompt rather than context the
/// harness injected (`<environment_context>`, `<user_instructions>`,
/// AGENTS.md contents, sub-agent notifications…).
fn is_prompt(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("# AGENTS.md")
}

/// The text items of a message's content, and how many images it held.
fn message_texts(content: Option<&Value>) -> (Vec<String>, usize) {
    let mut texts = Vec::new();
    let mut images = 0;
    match content {
        Some(Value::Array(items)) => {
            for item in items {
                match str_field(item, "type") {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(text) = str_field(item, "text") {
                            texts.push(text.to_owned());
                        }
                    }
                    Some("input_image" | "image") => images += 1,
                    _ => {}
                }
            }
        }
        Some(Value::String(text)) => texts.push(text.clone()),
        _ => {}
    }
    (texts, images)
}

/// A function call's arguments: Codex writes them as a JSON string, kept
/// parsed when they are an object or array (so blame and search see the
/// fields), as written otherwise.
fn parse_arguments(arguments: Option<&Value>) -> Value {
    match arguments {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => parsed,
            _ => Value::String(text.clone()),
        },
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// A tool output's text and exit code, when one can be read: a JSON
/// `{"output", "metadata": {"exit_code"}}` string (older builds and
/// apply_patch), a list of `input_text` items, or text whose lines say
/// `Process exited with code N` / `Exit code: N`.
fn output_text(output: Option<&Value>) -> (String, Option<i64>) {
    let text = match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match str_field(item, "type") {
                Some("input_text" | "output_text" | "text") => {
                    str_field(item, "text").unwrap_or_default().to_owned()
                }
                Some("input_image") => "[image omitted by traza import]".to_owned(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text) {
        if let Some(inner) = object.get("output").and_then(Value::as_str) {
            let exit = object
                .get("metadata")
                .and_then(|meta| meta.get("exit_code"))
                .and_then(Value::as_i64);
            return (inner.to_owned(), exit.or_else(|| exit_code_in(inner)));
        }
    }
    let exit = exit_code_in(&text);
    (text, exit)
}

/// An exit code stated on a line of its own near the top of `text`.
fn exit_code_in(text: &str) -> Option<i64> {
    for line in text.lines().take(12) {
        let line = line.trim();
        for prefix in ["Process exited with code ", "Exit code: ", "exit code: "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let digits: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Ok(code) = digits.parse() {
                    return Some(code);
                }
            }
        }
    }
    None
}

/// A prompt as a span event.
fn prompt_event(prompt: &str, at: u64) -> Event {
    let mut attributes = Map::new();
    attributes.insert("content".into(), json!(bounded(prompt, MAX_FIELD_BYTES)));
    Event {
        name: "gen_ai.user.message".into(),
        timestamp_ns: at,
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_read_from_every_observed_output_shape() {
        let json_shape =
            json!("{\"output\":\"Success. Updated a.rs\\n\",\"metadata\":{\"exit_code\":0}}");
        assert_eq!(
            output_text(Some(&json_shape)),
            ("Success. Updated a.rs\n".into(), Some(0))
        );
        let text =
            json!("Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code 2\nOutput:\nboom");
        assert_eq!(output_text(Some(&text)).1, Some(2));
        let list = json!([{"type": "input_text", "text": "Exit code: 1\nWall time: 3s"}]);
        assert_eq!(output_text(Some(&list)).1, Some(1));
        assert_eq!(output_text(Some(&json!("all good"))).1, None);
    }

    #[test]
    fn harness_context_is_not_a_prompt() {
        assert!(is_prompt("fix the failing test"));
        assert!(!is_prompt("<environment_context>cwd</environment_context>"));
        assert!(!is_prompt("# AGENTS.md instructions for /repo"));
        assert!(!is_prompt("   "));
    }

    #[test]
    fn arguments_stay_parsed_only_when_structured() {
        assert_eq!(
            parse_arguments(Some(&json!("{\"cmd\":\"ls\"}"))),
            json!({"cmd": "ls"})
        );
        assert_eq!(parse_arguments(Some(&json!("42"))), json!("42"));
        assert_eq!(parse_arguments(None), Value::Null);
    }

    #[test]
    fn a_thread_joins_its_roots_run() {
        let mut ancestry = Ancestry::default();
        ancestry
            .parents
            .insert("child".into(), Some("parent".into()));
        ancestry
            .parents
            .insert("parent".into(), Some("root".into()));
        ancestry.parents.insert("root".into(), None);
        assert_eq!(ancestry.root("grandchild", Some("child")), "root");
        assert_eq!(ancestry.root("root", None), "root");
        // A parent not on disk is as far as the walk can go.
        assert_eq!(ancestry.root("orphan", Some("gone")), "gone");
        // A cycle cannot hang the walk.
        ancestry.parents.insert("a".into(), Some("b".into()));
        ancestry.parents.insert("b".into(), Some("a".into()));
        let _ = ancestry.root("a", Some("b"));
    }
}
