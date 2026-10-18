//! Claude Code transcripts to spans. The observed format is documented on
//! the parent module; this file is the state machine that reads it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{
    blank_span, bounded, bounded_value, extend_end, preview, span_id, str_field, timestamp_ns,
    trace_id, Base, Converter, FileStats, Source, Spans, MAX_FIELD_BYTES, MAX_PART_BYTES,
    TURN_SPAN_NAME,
};
use crate::hash::{hash128, Hash128};
use crate::Event;

const SOURCE: Source = Source::ClaudeCode;

/// Input messages held for the next model call, at most. Older ones are
/// dropped (counted) — a run of hundreds of harness messages with no model
/// call between them is not a conversation delta anyone reads.
const MAX_DELTA_MESSAGES: usize = 256;

/// Parts kept in one response's output message.
const MAX_OUTPUT_PARTS: usize = 1024;

/// Record uuids remembered for their timestamps (a model call starts when
/// the record it continues from was written).
const MAX_REMEMBERED_RECORDS: usize = 1_000_000;

/// Record types that are bookkeeping: counted, not converted.
const BOOKKEEPING: &[&str] = &[
    "summary",
    "attachment",
    "ai-title",
    "custom-title",
    "last-prompt",
    "queue-operation",
    "mode",
    "pr-link",
    "file-history-snapshot",
    "file-history-delta",
    "agent-name",
    "cost-state",
    // Desktop-app bookkeeping, observed on real installations.
    "atis-latch",
    "bridge-session",
    "frame-link",
    "artifact-comment-monitor",
    "artifact-autoreact-ledger",
];

/// What kind of transcript a file is.
#[derive(Clone, Debug)]
enum Mode {
    /// A session's main transcript.
    Main,
    /// A sub-agent's transcript.
    Agent {
        agent_id: String,
        agent_type: Option<String>,
        description: Option<String>,
        tool_use_id: Option<String>,
        workflow: Option<String>,
    },
}

/// Converts one Claude Code transcript file.
pub(crate) struct ClaudeCode {
    tenant: String,
    spans: Spans,
    base: Base,
    trace: String,
    mode: Mode,
    /// The agent name every llm and tool span carries.
    agent_name: String,
    /// Agent mode: the agent span, once created.
    agent_span: Option<String>,
    /// Main mode: the current turn span.
    turn: Option<String>,
    goal_set: bool,
    any_span: bool,
    delta: Vec<Value>,
    /// message id → llm span id.
    messages: HashMap<String, String>,
    /// Content blocks already folded into each response, by digest, so a
    /// transcript that repeats earlier blocks in later records (older builds
    /// wrote cumulative content) does not double them.
    seen_blocks: HashSet<(String, Hash128)>,
    /// tool-use id → tool span id.
    tools: HashMap<String, String>,
    /// record uuid → its timestamp.
    times: HashMap<String, u64>,
    /// The last real model seen: a locally synthesized error reply names
    /// none, and the call it stands for was made with this one.
    last_model: Option<String>,
    /// Agent mode: the sub-agent's `.meta.json`, while it has not been read
    /// yet (Claude Code may write it after the transcript's first lines).
    pending_meta: Option<PathBuf>,
}

impl ClaudeCode {
    /// A converter for the transcript at `path`, stamping `tenant`.
    pub(crate) fn new(path: &Path, tenant: &str) -> Self {
        let (session, mode, meta_read) = classify(path);
        let pending_meta = match (&mode, meta_read) {
            (Mode::Agent { .. }, false) => Some(path.with_extension("meta.json")),
            _ => None,
        };
        let agent_name = match &mode {
            Mode::Main => SOURCE.as_str().to_owned(),
            Mode::Agent { agent_type, .. } => {
                agent_type.clone().unwrap_or_else(|| "subagent".to_owned())
            }
        };
        Self {
            tenant: tenant.to_owned(),
            spans: Spans::default(),
            trace: trace_id(SOURCE, &session),
            base: Base {
                session,
                ..Base::default()
            },
            mode,
            agent_name,
            agent_span: None,
            turn: None,
            goal_set: false,
            any_span: false,
            delta: Vec::new(),
            messages: HashMap::new(),
            seen_blocks: HashSet::new(),
            tools: HashMap::new(),
            times: HashMap::new(),
            last_model: None,
            pending_meta,
        }
    }

    /// Reads a sub-agent's meta file that was missing when the converter was
    /// built, and puts what it says onto the spans already built: the agent
    /// span gets its parent (the tool call that spawned it) and description,
    /// and every span its agent name. Touched at `end`, so they are sent
    /// again.
    fn read_pending_meta(&mut self, end: u64) {
        let Some(path) = &self.pending_meta else {
            return;
        };
        let Some(meta) = read_meta(path) else {
            return;
        };
        self.pending_meta = None;
        let Mode::Agent {
            agent_type,
            description,
            tool_use_id,
            ..
        } = &mut self.mode
        else {
            return;
        };
        let text = |key: &str| str_field(&meta, key).map(str::to_owned);
        if agent_type.is_none() {
            *agent_type = text("agentType");
        }
        if description.is_none() {
            *description = text("description");
        }
        if tool_use_id.is_none() {
            *tool_use_id = text("toolUseId");
        }
        let (agent_type, description, tool_use_id) =
            (agent_type.clone(), description.clone(), tool_use_id.clone());
        let old_name = self.agent_name.clone();
        if let Some(kind) = &agent_type {
            self.agent_name.clone_from(kind);
        }
        let new_name = self.agent_name.clone();
        let parent = tool_use_id.map(|tool| span_id(SOURCE, self.session(), "tool", &tool));
        let agent_span = self.agent_span.clone();
        self.spans.update_each(end, |span| {
            let mut changed = false;
            if Some(&span.span_id) == agent_span.as_ref() {
                if span.parent_span_id.is_none() && parent.is_some() {
                    span.parent_span_id.clone_from(&parent);
                    changed = true;
                }
                if let Some(description) = &description {
                    span.attributes
                        .insert("gen_ai.agent.description".into(), json!(description));
                    changed = true;
                }
                if let Some(kind) = &agent_type {
                    span.attributes
                        .insert("claude_code.agent_type".into(), json!(kind));
                    span.name = format!("invoke_agent {new_name}");
                    changed = true;
                }
            }
            if new_name != old_name
                && span
                    .attributes
                    .get("gen_ai.agent.name")
                    .and_then(Value::as_str)
                    == Some(old_name.as_str())
            {
                span.attributes
                    .insert("gen_ai.agent.name".into(), json!(new_name));
                changed = true;
            }
            changed
        });
    }

    fn session(&self) -> &str {
        &self.base.session
    }

    /// The span llm calls hang under: the turn (opening an implicit one when
    /// the transcript starts mid-conversation) or the agent span.
    fn container(&mut self, record: &Value, at: u64, end: u64) -> Option<String> {
        match &self.mode {
            Mode::Agent { .. } => self.agent(at, end),
            Mode::Main => {
                if self.turn.is_none() {
                    let key = format!("implicit:{}", str_field(record, "uuid").unwrap_or("start"));
                    self.open_turn(&key, at, end, None);
                }
                self.turn.clone()
            }
        }
    }

    /// The agent span (agent mode), created at the first record.
    fn agent(&mut self, at: u64, end: u64) -> Option<String> {
        if let Some(id) = &self.agent_span {
            let id = id.clone();
            if let Some(span) = self.spans.get_mut(&id, end) {
                extend_end(span, at);
            }
            return Some(id);
        }
        let Mode::Agent {
            agent_id,
            agent_type,
            description,
            tool_use_id,
            workflow,
        } = self.mode.clone()
        else {
            return None;
        };
        let id = span_id(SOURCE, self.session(), "agent", &agent_id);
        let parent = tool_use_id.map(|tool| span_id(SOURCE, self.session(), "tool", &tool));
        let name = format!("invoke_agent {}", self.agent_name);
        let (tenant, trace, base, agent_name) = (
            self.tenant.clone(),
            self.trace.clone(),
            self.base.clone(),
            self.agent_name.clone(),
        );
        let span = self.spans.touch(&id, end, || {
            let mut span = blank_span(SOURCE, &tenant, &trace, &id, parent, name, at);
            base.apply(SOURCE, &mut span.attributes);
            let a = &mut span.attributes;
            a.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
            a.insert("gen_ai.agent.name".into(), json!(agent_name));
            a.insert("gen_ai.agent.id".into(), json!(agent_id));
            if let Some(description) = description {
                a.insert("gen_ai.agent.description".into(), json!(description));
            }
            if let Some(kind) = agent_type {
                a.insert("claude_code.agent_type".into(), json!(kind));
            }
            if let Some(workflow) = workflow {
                a.insert("claude_code.workflow_id".into(), json!(workflow));
            }
            span
        });
        span?;
        self.any_span = true;
        self.agent_span = Some(id.clone());
        Some(id)
    }

    fn open_turn(&mut self, key: &str, at: u64, end: u64, prompt: Option<&str>) {
        let id = span_id(SOURCE, self.session(), "turn", key);
        let (tenant, trace, base) = (self.tenant.clone(), self.trace.clone(), self.base.clone());
        let goal = match (prompt, self.goal_set) {
            (Some(prompt), false) => Some(preview(prompt, 200)),
            _ => None,
        };
        let created = self
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
            .map(|span| {
                if let Some(prompt) = prompt {
                    span.events.push(prompt_event(prompt, at));
                }
                if let Some(goal) = &goal {
                    span.attributes.insert("session.goal".into(), json!(goal));
                }
            })
            .is_some();
        if created {
            self.any_span = true;
            if goal.is_some() {
                self.goal_set = true;
            }
            self.turn = Some(id);
        }
    }

    /// Extends the current turn or agent span to `at`.
    fn extend_container(&mut self, at: u64, end: u64) {
        let id = match &self.mode {
            Mode::Main => self.turn.clone(),
            Mode::Agent { .. } => self.agent_span.clone(),
        };
        if let Some(id) = id {
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

    fn user(&mut self, record: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let content = record.pointer("/message/content");
        let meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
        let compact = record.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
        let mut texts = Vec::new();
        let mut results = Vec::new();
        let mut images = 0usize;
        match content {
            Some(Value::String(text)) => texts.push(text.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match str_field(block, "type") {
                        Some("text") => {
                            if let Some(text) = str_field(block, "text") {
                                texts.push(text.to_owned());
                            }
                        }
                        Some("tool_result") => results.push(block),
                        Some("image") => images += 1,
                        _ => stats.unknown_blocks += 1,
                    }
                }
            }
            _ => {}
        }
        if !results.is_empty() {
            stats.record("user/tool_result");
            for block in results {
                self.tool_result(block, at, end, stats);
            }
            self.extend_container(at, end);
            return;
        }
        let text = texts.join("\n");
        let mut parts = vec![json!({"type": "text", "content": bounded(&text, MAX_PART_BYTES)})];
        for _ in 0..images {
            parts.push(json!({"type": "image", "omitted": true}));
        }
        let message = json!({"role": "user", "parts": parts});
        let interrupt = text.starts_with("[Request interrupted");
        if meta || compact || interrupt || text.trim().is_empty() && images == 0 {
            stats.record(if compact {
                "user/compact-summary"
            } else if interrupt {
                "user/interrupt"
            } else {
                "user/meta"
            });
            if interrupt {
                if let Some(id) = self.turn.clone() {
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        span.attributes
                            .insert("claude_code.interrupted".into(), json!(true));
                        span.events.push(Event {
                            name: "claude_code.interrupted".into(),
                            timestamp_ns: at,
                            attributes: Default::default(),
                        });
                    }
                }
            }
            self.push_delta(message, stats);
            self.extend_container(at, end);
            return;
        }
        stats.record("user/prompt");
        match &self.mode {
            Mode::Main => {
                let key = str_field(record, "uuid").unwrap_or("prompt").to_owned();
                self.open_turn(&key, at, end, Some(&text));
            }
            Mode::Agent { .. } => {
                if let Some(id) = self.agent(at, end) {
                    if let Some(span) = self.spans.get_mut(&id, end) {
                        span.events.push(prompt_event(&text, at));
                        span.attributes
                            .entry("input.value")
                            .or_insert_with(|| json!(bounded(&text, MAX_FIELD_BYTES)));
                    }
                }
            }
        }
        self.push_delta(message, stats);
    }

    fn tool_result(&mut self, block: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let id = str_field(block, "tool_use_id")
            .unwrap_or_default()
            .to_owned();
        let text = result_text(block.get("content"));
        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
        match self.tools.get(&id).cloned() {
            Some(span_id) => {
                if let Some(span) = self.spans.get_mut(&span_id, end) {
                    span.attributes.insert(
                        "gen_ai.tool.call.result".into(),
                        json!(bounded(&text, MAX_FIELD_BYTES)),
                    );
                    span.status = if is_error { "error" } else { "ok" }.to_owned();
                    extend_end(span, at);
                }
            }
            None => stats.record("tool_result/unmatched"),
        }
        self.push_delta(
            json!({"role": "tool", "parts": [{
                "type": "tool_call_response",
                "id": id,
                "response": bounded(&text, MAX_PART_BYTES),
            }]}),
            stats,
        );
    }

    fn assistant(&mut self, record: &Value, at: u64, end: u64, stats: &mut FileStats) {
        stats.record("assistant");
        let Some(message) = record.get("message") else {
            return;
        };
        let message_id = str_field(message, "id")
            .or_else(|| str_field(record, "uuid"))
            .unwrap_or("message")
            .to_owned();
        let reported = str_field(message, "model").filter(|model| *model != "<synthetic>");
        if let Some(model) = reported {
            self.last_model = Some(model.to_owned());
        }
        let api_error = record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
        let requested = reported
            .map(str::to_owned)
            .or_else(|| str_field(record, "requestedModel").map(str::to_owned))
            .or_else(|| self.last_model.clone());
        let model = requested.as_deref();
        let Some(container) = self.container(record, at, end) else {
            return;
        };
        let llm_id = match self.messages.get(&message_id) {
            Some(id) => id.clone(),
            None => {
                let id = span_id(SOURCE, self.session(), "llm", &message_id);
                let mut started = str_field(record, "parentUuid")
                    .and_then(|parent| self.times.get(parent).copied())
                    .filter(|started| *started <= at)
                    .unwrap_or(at);
                // A sub-agent's first call starts the instant its agent does;
                // a nanosecond later keeps the agent first in step order
                // (steps tie-break on ids, which would order them by chance).
                if let Some(agent_start) = self
                    .agent_span
                    .as_deref()
                    .and_then(|id| self.spans.start_of(id))
                {
                    if started <= agent_start {
                        started = agent_start + 1;
                    }
                }
                let input = std::mem::take(&mut self.delta);
                let (tenant, trace, base, agent) = (
                    self.tenant.clone(),
                    self.trace.clone(),
                    self.base.clone(),
                    self.agent_name.clone(),
                );
                let name = format!("chat {}", model.unwrap_or("unknown"));
                let made = self.spans.touch(&id, end, || {
                    let mut span =
                        blank_span(SOURCE, &tenant, &trace, &id, Some(container), name, started);
                    base.apply(SOURCE, &mut span.attributes);
                    let a = &mut span.attributes;
                    a.insert("gen_ai.operation.name".into(), json!("chat"));
                    a.insert("gen_ai.provider.name".into(), json!("anthropic"));
                    if let Some(model) = model {
                        a.insert("gen_ai.request.model".into(), json!(model));
                    }
                    a.insert("gen_ai.response.id".into(), json!(message_id));
                    a.insert("gen_ai.agent.name".into(), json!(agent));
                    if !input.is_empty() {
                        a.insert("gen_ai.input.messages".into(), Value::Array(input));
                    }
                    a.insert(
                        "gen_ai.output.messages".into(),
                        json!([{"role": "assistant", "parts": []}]),
                    );
                    span
                });
                if made.is_none() {
                    return;
                }
                self.any_span = true;
                self.messages.insert(message_id.clone(), id.clone());
                id
            }
        };
        // Content blocks: parts of the response, and tool calls.
        let mut new_parts = Vec::new();
        let mut last_text = None;
        if let Some(Value::Array(blocks)) = message.get("content") {
            for block in blocks {
                let digest = hash128(block.to_string().as_bytes());
                if !self.seen_blocks.insert((message_id.clone(), digest)) {
                    continue;
                }
                match str_field(block, "type") {
                    Some("text") => {
                        let text = str_field(block, "text").unwrap_or_default();
                        last_text = Some(text.to_owned());
                        new_parts.push(
                            json!({"type": "text", "content": bounded(text, MAX_PART_BYTES)}),
                        );
                    }
                    Some("thinking") => {
                        let text = str_field(block, "thinking").unwrap_or_default();
                        if !text.is_empty() {
                            new_parts.push(json!({"type": "reasoning", "content": bounded(text, MAX_PART_BYTES)}));
                        }
                    }
                    Some("redacted_thinking") => {}
                    Some("tool_use") => {
                        let tool_use = str_field(block, "id").unwrap_or_default().to_owned();
                        let name = str_field(block, "name").unwrap_or("tool").to_owned();
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        new_parts.push(json!({
                            "type": "tool_call",
                            "id": tool_use,
                            "name": name,
                            "arguments": bounded_value(&input, MAX_PART_BYTES),
                        }));
                        self.tool_call(&llm_id, &tool_use, &name, &input, at, end);
                    }
                    Some(other) => {
                        stats.unknown_blocks += 1;
                        new_parts.push(json!({"type": other}));
                    }
                    None => stats.unknown_blocks += 1,
                }
            }
        }
        let status = if api_error { "error" } else { "ok" };
        let finish = str_field(message, "stop_reason").map(finish_reason);
        let usage = message.get("usage").cloned();
        let error_text = str_field(record, "error").map(str::to_owned).or_else(|| {
            record
                .get("apiErrorStatus")
                .and_then(Value::as_u64)
                .map(|code| code.to_string())
        });
        if let Some(span) = self.spans.get_mut(&llm_id, end) {
            extend_end(span, at);
            if let Some(Value::Array(messages)) = span.attributes.get_mut("gen_ai.output.messages")
            {
                if let Some(Value::Object(output)) = messages.first_mut() {
                    if let Some(Value::Array(parts)) = output.get_mut("parts") {
                        for part in new_parts {
                            if parts.len() < MAX_OUTPUT_PARTS {
                                parts.push(part);
                            }
                        }
                    }
                    if let Some(finish) = finish {
                        output.insert("finish_reason".into(), json!(finish));
                    }
                }
            }
            if let Some(usage) = usage.as_ref() {
                set_usage(&mut span.attributes, usage);
            }
            if let Some(model) = reported {
                span.attributes
                    .insert("gen_ai.response.model".into(), json!(model));
            }
            if api_error {
                span.status = status.to_owned();
                span.attributes.insert(
                    "error.type".into(),
                    json!(error_text.unwrap_or_else(|| "api_error".to_owned())),
                );
            }
        }
        // The container ends with its last record and takes the status of
        // its last model call.
        let container = match &self.mode {
            Mode::Main => self.turn.clone(),
            Mode::Agent { .. } => self.agent_span.clone(),
        };
        if let Some(container) = container {
            if let Some(span) = self.spans.get_mut(&container, end) {
                extend_end(span, at);
                span.status = status.to_owned();
                if let (Mode::Agent { .. }, Some(text)) = (&self.mode, &last_text) {
                    span.attributes
                        .insert("output.value".into(), json!(bounded(text, MAX_FIELD_BYTES)));
                }
            }
        }
    }

    fn tool_call(
        &mut self,
        llm: &str,
        tool_use: &str,
        name: &str,
        input: &Value,
        at: u64,
        end: u64,
    ) {
        let id = span_id(SOURCE, self.session(), "tool", tool_use);
        if self.spans.contains(&id) {
            return;
        }
        let (tenant, trace, base, agent) = (
            self.tenant.clone(),
            self.trace.clone(),
            self.base.clone(),
            self.agent_name.clone(),
        );
        let parent = Some(llm.to_owned());
        let made = self.spans.touch(&id, end, || {
            let mut span = blank_span(
                SOURCE,
                &tenant,
                &trace,
                &id,
                parent,
                format!("execute_tool {name}"),
                at,
            );
            base.apply(SOURCE, &mut span.attributes);
            let a = &mut span.attributes;
            a.insert("gen_ai.operation.name".into(), json!("execute_tool"));
            a.insert("gen_ai.tool.name".into(), json!(name));
            a.insert("gen_ai.tool.call.id".into(), json!(tool_use));
            a.insert(
                "gen_ai.tool.call.arguments".into(),
                bounded_value(input, MAX_FIELD_BYTES),
            );
            a.insert("gen_ai.agent.name".into(), json!(agent));
            span
        });
        if made.is_some() {
            self.tools.insert(tool_use.to_owned(), id);
        }
    }

    fn system(&mut self, record: &Value, at: u64, end: u64, stats: &mut FileStats) {
        let subtype = str_field(record, "subtype").unwrap_or("other");
        stats.record(&format!("system/{subtype}"));
        let event = match subtype {
            "api_error" => Some(Event {
                name: "claude_code.api_error".into(),
                timestamp_ns: at,
                attributes: [
                    ("retry_attempt", record.get("retryAttempt")),
                    ("max_retries", record.get("maxRetries")),
                ]
                .into_iter()
                .filter_map(|(key, value)| Some((key.to_owned(), value?.clone())))
                .collect(),
            }),
            "compact_boundary" => Some(Event {
                name: "claude_code.compaction".into(),
                timestamp_ns: at,
                attributes: record
                    .get("compactMetadata")
                    .and_then(Value::as_object)
                    .map(|meta| {
                        meta.iter()
                            .filter(|(_, value)| !value.is_object() && !value.is_array())
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        };
        if let Some(event) = event {
            let id = match &self.mode {
                Mode::Main => self.turn.clone(),
                Mode::Agent { .. } => self.agent_span.clone(),
            };
            if let Some(id) = id {
                if let Some(span) = self.spans.get_mut(&id, end) {
                    if span.events.len() < 1000 {
                        span.events.push(event);
                    }
                    extend_end(span, at);
                }
            }
        }
    }
}

impl Converter for ClaudeCode {
    fn feed(&mut self, line: &[u8], end: u64, stats: &mut FileStats) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            stats.malformed_lines += 1;
            return;
        };
        if !record.is_object() {
            stats.malformed_lines += 1;
            return;
        }
        let kind = str_field(&record, "type").unwrap_or("<none>").to_owned();
        if !self.any_span {
            // A transcript names its own session; prefer that to the path's
            // guess while nothing has been derived from the guess yet.
            if let Some(session) = str_field(&record, "sessionId") {
                if session != self.base.session {
                    self.base.session = session.to_owned();
                    self.trace = trace_id(SOURCE, session);
                }
            }
        }
        if let Some(cwd) = str_field(&record, "cwd") {
            if self.base.project.is_none() {
                self.base.project = Some(cwd.to_owned());
            }
        }
        if let Some(version) = str_field(&record, "version") {
            self.base.version = Some(version.to_owned());
        }
        if let Some(branch) = str_field(&record, "gitBranch").filter(|b| !b.is_empty()) {
            self.base.branch = Some(branch.to_owned());
        }
        let at = timestamp_ns(&record);
        let sidechain_inline = matches!(self.mode, Mode::Main)
            && record.get("isSidechain").and_then(Value::as_bool) == Some(true);
        match (kind.as_str(), at) {
            // Older builds wrote sub-agent records inline in the main file;
            // they cannot be placed without the agent's own transcript.
            ("user" | "assistant", _) if sidechain_inline => stats.record("sidechain-inline"),
            ("user", Some(at)) => self.user(&record, at, end, stats),
            ("assistant", Some(at)) => self.assistant(&record, at, end, stats),
            ("system", Some(at)) => self.system(&record, at, end, stats),
            ("user" | "assistant" | "system", None) => stats.record("untimed"),
            (other, _) if BOOKKEEPING.contains(&other) => stats.record(other),
            (other, _) => stats.unknown(other),
        }
        if let (Some(uuid), Some(at)) = (str_field(&record, "uuid"), at) {
            if self.times.len() < MAX_REMEMBERED_RECORDS {
                self.times.insert(uuid.to_owned(), at);
            }
        }
    }

    fn spans(&mut self) -> &mut Spans {
        &mut self.spans
    }

    fn run(&self) -> Option<String> {
        Some(self.base.session.clone())
    }

    fn wants_refresh(&self) -> bool {
        self.pending_meta.is_some()
    }

    fn refresh(&mut self, end: u64) {
        self.read_pending_meta(end);
    }
}

/// A sub-agent's `.meta.json`, when it exists and parses.
fn read_meta(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
}

/// The session and mode a path implies: `<session>.jsonl` is a main
/// transcript; `<session>/subagents/[workflows/<wf>/]agent-<id>.jsonl` a
/// sub-agent's, with its `.meta.json` beside it. The flag says whether a
/// sub-agent's meta file was there to read.
fn classify(path: &Path) -> (String, Mode, bool) {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_owned();
    let ancestors: Vec<PathBuf> = path.ancestors().map(Path::to_path_buf).collect();
    let subagents = ancestors
        .iter()
        .position(|dir| dir.file_name().and_then(|name| name.to_str()) == Some("subagents"));
    let (Some(agent_id), Some(at)) = (stem.strip_prefix("agent-"), subagents) else {
        return (stem, Mode::Main, true);
    };
    let session = ancestors
        .get(at + 1)
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("session")
        .to_owned();
    let workflow = path
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with("wf_"))
        .map(str::to_owned);
    let meta = read_meta(&path.with_extension("meta.json"));
    let meta_read = meta.is_some();
    let meta = meta.unwrap_or(Value::Null);
    let text = |key: &str| str_field(&meta, key).map(str::to_owned);
    (
        session,
        Mode::Agent {
            agent_id: agent_id.to_owned(),
            agent_type: text("agentType"),
            description: text("description"),
            tool_use_id: text("toolUseId"),
            workflow,
        },
        meta_read,
    )
}

/// A prompt as a span event.
fn prompt_event(prompt: &str, at: u64) -> Event {
    let mut attributes = serde_json::Map::new();
    attributes.insert("content".into(), json!(bounded(prompt, MAX_FIELD_BYTES)));
    Event {
        name: "gen_ai.user.message".into(),
        timestamp_ns: at,
        attributes,
    }
}

/// A tool result's text: a string, or the text blocks of a list with
/// images replaced by a placeholder.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match str_field(block, "type") {
                Some("text") => str_field(block, "text").unwrap_or_default().to_owned(),
                Some("image") => "[image omitted by traza import]".to_owned(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// Anthropic's stop reason in OTel's finish-reason vocabulary.
fn finish_reason(stop: &str) -> &str {
    match stop {
        "tool_use" => "tool_call",
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        other => other,
    }
}

/// Anthropic usage as OTel GenAI token attributes. `input_tokens` is the
/// uncached remainder, so the cache counters ride beside it (and
/// `gen_ai.provider.name=anthropic` tells readers to add them).
fn set_usage(attributes: &mut serde_json::Map<String, Value>, usage: &Value) {
    for (from, to) in [
        ("input_tokens", "gen_ai.usage.input_tokens"),
        ("output_tokens", "gen_ai.usage.output_tokens"),
        (
            "cache_read_input_tokens",
            "gen_ai.usage.cache_read.input_tokens",
        ),
        (
            "cache_creation_input_tokens",
            "gen_ai.usage.cache_creation.input_tokens",
        ),
    ] {
        if let Some(count) = usage.get(from).and_then(Value::as_u64) {
            attributes.insert(to.into(), json!(count));
        }
    }
}
