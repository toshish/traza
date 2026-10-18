//! Zero-setup recorders: coding-agent transcripts imported as runs.
//!
//! Claude Code and the Codex CLI already write down everything they do — one
//! JSONL transcript per session, on the developer's own disk. This module
//! turns those transcripts into spans (sessions become runs, model responses
//! become llm steps, tool calls become tool steps, sub-agents become agent
//! spans) and ingests them, either over HTTP into a running server or
//! straight into a data directory. Nothing has to be instrumented: point
//! `traza-server import claude-code` at `~/.claude/projects` and every
//! session the machine ever ran is a run Traza can tape, diff, recall, guard
//! and blame. With `--watch` it keeps following the files, so a live session
//! is a live run within about a second.
//!
//! # Determinism is what makes re-import safe
//!
//! Every id is derived, never minted: a trace id is the first 32 hex
//! characters, and a span id the first 16, of
//! `sha256("traza-import/1\0" + source + "\0" + session + "\0" + kind + "\0" + key)`,
//! where `key` is the transcript's own identity for the item (a message id, a
//! tool-use id, a record uuid, an agent id, a Codex call id or line number).
//! Converting the same lines therefore produces byte-identical spans, and the
//! store's primary key is last-write-wins, so importing a transcript twice
//! leaves exactly one copy of each span. The derivation is keyed by the
//! session rather than by the file path (the path is what the contract first
//! suggested): a sub-agent's transcript lives in another file, and its agent
//! span must name its parent tool span — which only works if both sides can
//! derive that id from data they both hold. It also keeps ids stable when
//! `~/.claude` moves to another machine.
//!
//! # Resuming without re-sending
//!
//! A converter keeps, for every span it built, the end offset of the last
//! transcript line that contributed to it. The state file records, per
//! destination and per file, how far the file has been read and sent. A
//! file is re-read from its start (context such as the current turn lives in
//! earlier lines), and only spans touched by a line at or past the recorded
//! offset are sent: a span built entirely from earlier lines was already sent
//! in exactly its final form. A file that shrank, or whose first bytes
//! changed, is treated as new. `--watch` keeps converters in memory while
//! files are active, so following a live file reads only its new lines.
//!
//! # What the transcripts look like (observed, documented)
//!
//! The formats are undocumented and versioned by their tools, so the
//! importer reads them leniently: a record it does not know is counted under
//! its type and skipped, never fatal, and a line that is not JSON is counted
//! as malformed. What follows is what was observed on real installations
//! (Claude Code 2.x, Codex CLI 0.100–0.160); the committed fixtures are
//! synthetic and follow these shapes.
//!
//! **Claude Code** — `~/.claude/projects/<mangled-cwd>/<sessionId>.jsonl`,
//! one JSON object per line, `sessionId` equal to the file stem. Records
//! carry `type`, `uuid`, `parentUuid`, `timestamp` (RFC 3339), `sessionId`,
//! `cwd`, `version`, `gitBranch` and `isSidechain`:
//! - `user` — `message.content` is a string (a prompt) or a list of blocks:
//!   `text`, `image`, and `tool_result` (`tool_use_id`, `content` as a string
//!   or a list of `text`/`image` blocks, optional `is_error`). `isMeta` marks
//!   harness-injected text; `isCompactSummary` the summary after compaction.
//!   `toolUseResult` holds the tool's structured result (not needed here).
//! - `assistant` — `message` is an Anthropic message (`id`, `model`,
//!   `content`, `stop_reason`, `usage` with `input_tokens`,
//!   `output_tokens`, `cache_read_input_tokens`,
//!   `cache_creation_input_tokens`). **One API response is written as several
//!   records, one content block each, sharing `message.id` and repeating the
//!   same `usage`** — so records are grouped by `message.id` and usage is
//!   counted once per response, never per record. Blocks are `text`,
//!   `thinking`, `tool_use` (`id`, `name`, `input`). `isApiErrorMessage`
//!   marks a locally synthesized error reply (model `<synthetic>`).
//! - `system` — `subtype` `api_error` (retries), `compact_boundary`,
//!   `stop_hook_summary`, `local_command`, …; `attachment`, `summary`,
//!   `ai-title`, `custom-title`, `last-prompt`, `queue-operation`, `mode`,
//!   `pr-link`, `file-history-*`, `agent-name` and others are bookkeeping.
//! - Sub-agents (the `Agent` tool, formerly `Task`) write
//!   `<sessionId>/subagents/agent-<agentId>.jsonl` (records with
//!   `isSidechain: true`, `agentId`, the parent's `sessionId`) beside
//!   `agent-<agentId>.meta.json` (`agentType`, `description`, `toolUseId` —
//!   the parent's tool-use id). Workflow agents live under
//!   `subagents/workflows/<wf_id>/` with a meta file that names no tool use.
//!
//! **Codex CLI** — `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//! Each line is `{"timestamp", "type", "payload"}` (newer builds add
//! `ordinal`):
//! - `session_meta` — `id` (the thread id), `cwd`, `cli_version`,
//!   `model_provider`, `git`, and for a sub-agent thread
//!   `parent_thread_id` / `source.subagent.thread_spawn` with
//!   `agent_nickname` and `agent_role`.
//! - `turn_context` — `turn_id`, `cwd`, `model`; `event_msg` with payload
//!   `task_started` / `task_complete` (`error`, `last_agent_message`) /
//!   `turn_aborted` delimit turns; `event_msg/token_count` carries
//!   `info.last_token_usage` (`input_tokens` including
//!   `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`) once
//!   per model response, often repeated with an unchanged total.
//! - `response_item` — `message` (`role` user / assistant / developer,
//!   `content` of `input_text` / `output_text` / `input_image`),
//!   `reasoning` (`summary`), `function_call` (`name`, `arguments` as a JSON
//!   string, `call_id`) and `function_call_output` (`call_id`, `output`),
//!   `custom_tool_call` (`name` such as `apply_patch`, `input`) and its
//!   output, `web_search_call`, `tool_search_call` / `_output`,
//!   `local_shell_call` (older builds). A model response is the run of
//!   output items between two inputs.
//! - `event_msg/item_completed` (`item.type` `Reasoning`, `AgentMessage`,
//!   `UserMessage`, `CommandExecution` with `command`/`exit_code`/
//!   `aggregated_output`, `FileChange` with `changes: {path: {type: add |
//!   update, content | unified_diff}}`, `McpToolCall`, `WebSearch`, …)
//!   usually duplicates the response items, but newer builds record some
//!   turns ONLY this way. Items are held per turn and replayed when the turn
//!   recorded no response items (see `codex::Codex::settle`).
//! - `compacted`, `world_state`, `token_usage_record`,
//!   `inter_agent_communication_metadata` and other event types are
//!   bookkeeping (`token_usage_record` duplicates `token_count`, and reading
//!   both would double-count).
//!
//! # The spans
//!
//! One trace per session (sub-agents included), so every parent link can be
//! derived. Every span carries `session.id` (Claude Code's `sessionId`, or
//! the Codex root thread id), `traza.source`, `code.project` (the session's
//! working directory), `service.version` (the tool's version) and, when
//! known, `vcs.ref.head.name`. The service is `claude-code` or `codex`.
//! - a **turn** span per user prompt (`gen_ai.user.message` event carrying
//!   the prompt; the session's first prompt, cut to 200 characters, is also
//!   `session.goal`); its status is that of its last model call;
//! - an **llm** step per model response (`gen_ai.operation.name=chat`,
//!   request model, provider, token usage including cache counters,
//!   `gen_ai.input.messages` = the conversation delta since the previous
//!   response, `gen_ai.output.messages` = the response's content), child of
//!   the turn (or of the agent span inside a sub-agent);
//! - a **tool** step per tool call (`gen_ai.tool.name`, `.call.id`,
//!   `.call.arguments` as JSON, `.call.result`, status `error` when the
//!   result says so), child of the llm step that requested it;
//! - an **agent** span per sub-agent (`gen_ai.agent.name`), child of the
//!   tool call that spawned it when the transcript names it.
//!
//! Text is bounded: a tool result or argument string past
//! [`MAX_FIELD_BYTES`] and a message part past [`MAX_PART_BYTES`] is cut with
//! a marker saying how much was dropped, and image data is replaced by a
//! placeholder naming its size — screenshots would otherwise dominate the
//! store. No cost is invented: tokens are recorded and the server's pricing
//! table prices them, exactly as for any other span.

mod claude_code;
mod cli;
mod codex;
mod sink;
mod state;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::Span;

pub use cli::{blame_main, import_main};
pub use sink::{http_request, DrySink, HttpSink, HttpTarget, SendError, Sink, StoreSink};

/// Longest single text value kept in a span (a tool result, an argument
/// string, a prompt), in bytes. Longer text is cut with a marker.
pub const MAX_FIELD_BYTES: usize = 1024 * 1024;

/// Longest text kept in one message part of `gen_ai.input.messages` or
/// `gen_ai.output.messages`, in bytes. The input delta repeats tool results
/// the tool spans already hold in full, so it is held tighter.
pub const MAX_PART_BYTES: usize = 64 * 1024;

/// Most spans one transcript file may produce. A file past it stops adding
/// new items (counted as `capped`); spans already built keep updating.
pub const MAX_SPANS_PER_FILE: usize = 200_000;

/// Longest transcript line read, in bytes. A longer line is skipped and
/// counted as malformed rather than held in memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Distinct record-type names counted individually in the summary; the rest
/// fold into `other`.
const MAX_TYPE_NAMES: usize = 64;

/// Converters kept in memory by `--watch`. Past it, the least recently
/// active is dropped; it is rebuilt (with resend suppression) if its file
/// grows again.
pub const MAX_LIVE_CONVERTERS: usize = 64;

/// How long `--watch` keeps an idle file's converter in memory.
pub const LIVE_IDLE: Duration = Duration::from_secs(600);

/// Spans per ingest request.
const BATCH_SPANS: usize = 500;

/// Serialized bytes per ingest request (well under the server's 64 MiB).
const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Which agent wrote the transcripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Anthropic's Claude Code.
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
}

impl Source {
    /// Parses `claude-code` or `codex`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The wire name, also the spans' `service` and `traza.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// The default transcript root under `home`.
    pub fn default_root(self, home: &Path) -> PathBuf {
        match self {
            Self::ClaudeCode => home.join(".claude").join("projects"),
            Self::Codex => home.join(".codex").join("sessions"),
        }
    }
}

// ------------------------------------------------------------------- ids

/// Derives a hex id of `hex_len` characters from the item's identity; see
/// the module docs for the construction.
pub fn derive_id(source: Source, session: &str, kind: &str, key: &str, hex_len: usize) -> String {
    let preimage = format!(
        "traza-import/1\0{}\0{session}\0{kind}\0{key}",
        source.as_str()
    );
    let digest = crate::payload::sha256_hex(preimage.as_bytes());
    digest[..hex_len.min(digest.len())].to_owned()
}

/// The trace id of a session.
pub fn trace_id(source: Source, session: &str) -> String {
    derive_id(source, session, "trace", "", 32)
}

/// The span id of one item of a session.
pub fn span_id(source: Source, session: &str, kind: &str, key: &str) -> String {
    derive_id(source, session, kind, key, 16)
}

// ------------------------------------------------------------ span store

/// One span a converter built, and where in its file it was last touched.
#[derive(Clone, Debug)]
pub(crate) struct Built {
    pub(crate) span: Span,
    /// End offset of the last line that changed this span.
    pub(crate) touched: u64,
    /// Changed since the last drain.
    pub(crate) dirty: bool,
}

/// The spans one file has produced, by span id.
#[derive(Debug, Default)]
pub(crate) struct Spans {
    map: HashMap<String, Built>,
    /// Creation order, so a drain is deterministic.
    order: Vec<String>,
    /// Items refused because the file reached [`MAX_SPANS_PER_FILE`].
    pub(crate) capped: u64,
}

impl Spans {
    /// Whether `id` exists.
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
    }

    /// The span `id`, created from `make` when absent (unless the file is at
    /// its cap, in which case `None`), marked touched at `offset`.
    pub(crate) fn touch(
        &mut self,
        id: &str,
        offset: u64,
        make: impl FnOnce() -> Span,
    ) -> Option<&mut Span> {
        if !self.map.contains_key(id) {
            if self.map.len() >= MAX_SPANS_PER_FILE {
                self.capped += 1;
                return None;
            }
            self.order.push(id.to_owned());
            self.map.insert(
                id.to_owned(),
                Built {
                    span: make(),
                    touched: offset,
                    dirty: true,
                },
            );
        }
        let built = self.map.get_mut(id)?;
        built.touched = built.touched.max(offset);
        built.dirty = true;
        Some(&mut built.span)
    }

    /// The span `id`, marked touched, if it exists.
    pub(crate) fn get_mut(&mut self, id: &str, offset: u64) -> Option<&mut Span> {
        let built = self.map.get_mut(id)?;
        built.touched = built.touched.max(offset);
        built.dirty = true;
        Some(&mut built.span)
    }

    /// The start time of span `id`, if it exists.
    pub(crate) fn start_of(&self, id: &str) -> Option<u64> {
        self.map.get(id).map(|built| built.span.start_time_ns)
    }

    /// Every span changed since the last drain whose last touch is past
    /// `floor`, in creation order; all dirty marks are cleared. A touch is
    /// the END offset of a line, and `floor` the end of the last line sent,
    /// so a span last touched exactly at `floor` was already sent.
    pub(crate) fn drain(&mut self, floor: u64) -> Vec<Span> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(built) = self.map.get_mut(id) {
                if built.dirty {
                    built.dirty = false;
                    if built.touched > floor {
                        out.push(built.span.clone());
                    }
                }
            }
        }
        out
    }

    /// Every span, in creation order.
    pub(crate) fn all(&self) -> impl Iterator<Item = &Span> {
        self.order
            .iter()
            .filter_map(|id| self.map.get(id))
            .map(|b| &b.span)
    }

    /// Number of spans.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// What a converter reads lines into.
pub(crate) trait Converter {
    /// Consumes one complete line (without its newline) that ends at byte
    /// `end` of the file.
    fn feed(&mut self, line: &[u8], end: u64, stats: &mut FileStats);
    /// The file is complete (a compressed rollout is final): settle anything
    /// held for a turn that will now never end. `end` is past the last line.
    fn finish(&mut self, _end: u64, _stats: &mut FileStats) {}
    /// The spans built so far.
    fn spans(&mut self) -> &mut Spans;
    /// The run id this file's spans carry, once known.
    fn run(&self) -> Option<String>;
}

/// A blank span with the identity fields set.
pub(crate) fn blank_span(
    source: Source,
    tenant: &str,
    trace_id: &str,
    span_id: &str,
    parent: Option<String>,
    name: String,
    at_ns: u64,
) -> Span {
    Span {
        trace_id: trace_id.to_owned(),
        span_id: span_id.to_owned(),
        tenant: tenant.to_owned(),
        parent_span_id: parent,
        name,
        start_time_ns: at_ns,
        end_time_ns: at_ns,
        status: "ok".to_owned(),
        service: source.as_str().to_owned(),
        attributes: Map::new(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

/// Extends a span's end to `at_ns` (never shrinks it, never before start).
pub(crate) fn extend_end(span: &mut Span, at_ns: u64) {
    if at_ns > span.end_time_ns {
        span.end_time_ns = at_ns;
    }
    if span.end_time_ns < span.start_time_ns {
        span.end_time_ns = span.start_time_ns;
    }
}

/// The base attributes every span of a session carries.
#[derive(Clone, Debug, Default)]
pub(crate) struct Base {
    pub(crate) session: String,
    pub(crate) project: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) branch: Option<String>,
}

impl Base {
    /// Writes the base attributes into `attributes`.
    pub(crate) fn apply(&self, source: Source, attributes: &mut Map<String, Value>) {
        attributes.insert("session.id".into(), json!(self.session));
        attributes.insert("traza.source".into(), json!(source.as_str()));
        if let Some(project) = &self.project {
            attributes.insert("code.project".into(), json!(project));
        }
        if let Some(version) = &self.version {
            attributes.insert("service.version".into(), json!(version));
        }
        if let Some(branch) = &self.branch {
            attributes.insert("vcs.ref.head.name".into(), json!(branch));
        }
    }
}

// ------------------------------------------------------------------ text

/// `text` cut to at most `max` bytes at a character boundary, with a marker
/// naming how many bytes were dropped.
pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…[traza import: {} more bytes not kept]",
        &text[..end],
        text.len() - end
    )
}

/// The first `max` characters of `text`, with an ellipsis when cut.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push('…');
    }
    out
}

/// A JSON value with every string bounded to `max` bytes and every
/// base64-looking image payload replaced by a placeholder, recursively.
pub(crate) fn bounded_value(value: &Value, max: usize) -> Value {
    match value {
        Value::String(text) => Value::String(bounded(text, max)),
        Value::Array(items) => Value::Array(items.iter().map(|v| bounded_value(v, max)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                out.insert(key.clone(), bounded_value(item, max));
            }
            out.into()
        }
        other => other.clone(),
    }
}

/// Parses an RFC 3339 timestamp field to Unix nanoseconds.
pub(crate) fn timestamp_ns(record: &Value) -> Option<u64> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| crate::mcp::parse_iso(text).ok())
}

/// A string field.
pub(crate) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

// ---------------------------------------------------------------- stats

/// Counts from converting one or more files.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct FileStats {
    /// Lines read.
    pub lines: u64,
    /// Lines that were not a JSON object (or were over [`MAX_LINE_BYTES`]).
    pub malformed_lines: u64,
    /// Records converted or deliberately ignored, by type.
    pub records: BTreeMap<String, u64>,
    /// Records of a type the importer does not know, by type. Skipped.
    pub unknown_records: BTreeMap<String, u64>,
    /// Content blocks or items of an unknown kind inside known records.
    pub unknown_blocks: u64,
    /// Items not converted because the file hit [`MAX_SPANS_PER_FILE`].
    pub capped: u64,
}

impl FileStats {
    pub(crate) fn record(&mut self, kind: &str) {
        bump(&mut self.records, kind);
    }

    pub(crate) fn unknown(&mut self, kind: &str) {
        bump(&mut self.unknown_records, kind);
    }

    fn merge(&mut self, other: &FileStats) {
        self.lines += other.lines;
        self.malformed_lines += other.malformed_lines;
        for (key, value) in &other.records {
            *entry(&mut self.records, key) += value;
        }
        for (key, value) in &other.unknown_records {
            *entry(&mut self.unknown_records, key) += value;
        }
        self.unknown_blocks += other.unknown_blocks;
        self.capped += other.capped;
    }
}

fn entry<'a>(map: &'a mut BTreeMap<String, u64>, key: &str) -> &'a mut u64 {
    if !map.contains_key(key) && map.len() >= MAX_TYPE_NAMES {
        return map.entry("other".to_owned()).or_insert(0);
    }
    map.entry(key.to_owned()).or_insert(0)
}

fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    *entry(map, key) += 1;
}

/// What a whole import did.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct ImportStats {
    /// The source.
    pub source: Option<Source>,
    /// Transcript files found under the root.
    pub files_seen: u64,
    /// Files read and converted this time.
    pub files_read: u64,
    /// Files skipped as unchanged since the state file's record.
    pub files_unchanged: u64,
    /// Files excluded by `--since` or `--project-filter`.
    pub files_filtered: u64,
    /// Files that could not be read (named in `errors`).
    pub files_failed: u64,
    /// Distinct sessions (runs) the converted files belong to.
    pub sessions: u64,
    /// Spans built across the converted files.
    pub spans_built: u64,
    /// Spans handed to the destination.
    pub spans_sent: u64,
    /// Spans the destination refused (named in `errors`).
    pub spans_refused: u64,
    /// Turn spans built.
    pub turns: u64,
    /// LLM steps built.
    pub llm_steps: u64,
    /// Tool steps built.
    pub tool_steps: u64,
    /// Tool steps whose status is `error`.
    pub tool_errors: u64,
    /// Agent spans built.
    pub agent_spans: u64,
    /// Input tokens across the LLM steps built.
    pub input_tokens: u64,
    /// Output tokens across the LLM steps built.
    pub output_tokens: u64,
    /// Cache-read tokens across the LLM steps built.
    pub cache_read_tokens: u64,
    /// Cache-creation tokens across the LLM steps built.
    pub cache_creation_tokens: u64,
    /// Line and record counts.
    pub transcript: FileStats,
    /// The first few errors, verbatim.
    pub errors: Vec<String>,
}

/// Errors kept verbatim in [`ImportStats::errors`].
const MAX_ERRORS: usize = 16;

impl ImportStats {
    pub(crate) fn error(&mut self, message: String) {
        if self.errors.len() < MAX_ERRORS && !self.errors.contains(&message) {
            self.errors.push(message);
        }
    }

    /// Adds the shape of one converted file's spans.
    fn tally(&mut self, spans: &Spans) {
        self.spans_built += spans.len() as u64;
        for span in spans.all() {
            let kind = span
                .attributes
                .get("gen_ai.operation.name")
                .and_then(Value::as_str);
            let get = |key: &str| {
                span.attributes
                    .get(key)
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            match kind {
                Some("chat") => {
                    self.llm_steps += 1;
                    self.input_tokens += get("gen_ai.usage.input_tokens");
                    self.output_tokens += get("gen_ai.usage.output_tokens");
                    self.cache_read_tokens += get("gen_ai.usage.cache_read.input_tokens");
                    self.cache_creation_tokens += get("gen_ai.usage.cache_creation.input_tokens");
                }
                Some("execute_tool") => {
                    self.tool_steps += 1;
                    if span.status == "error" {
                        self.tool_errors += 1;
                    }
                }
                Some("invoke_agent") => self.agent_spans += 1,
                _ if span.name == TURN_SPAN_NAME => self.turns += 1,
                _ => {}
            }
        }
    }
}

/// The operation name of a turn span.
pub(crate) const TURN_SPAN_NAME: &str = "turn";

// ------------------------------------------------------------- discovery

/// A transcript file and what is known of it before reading.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) modified: SystemTime,
    /// A zstd-compressed transcript (`.jsonl.zst`).
    pub(crate) compressed: bool,
}

/// The suffix of a zstd-compressed transcript.
pub(crate) const COMPRESSED_SUFFIX: &str = ".jsonl.zst";

/// Largest decompressed transcript read, in bytes. A compressed rollout is
/// decompressed whole (its lines are then read from memory), so this bounds
/// the memory one file can take.
pub const MAX_DECOMPRESSED_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// The first lines of a transcript (decompressing only a prefix of a
/// compressed one), for reading its header without reading all of it.
pub(crate) fn head_lines(path: &Path, max_lines: usize) -> Vec<String> {
    const HEAD_BYTES: u64 = 4 * 1024 * 1024;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut raw = Vec::new();
    if file.take(HEAD_BYTES).read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    let text = if crate::zstd::is_zstd(&raw) {
        crate::zstd::decompress_prefix(&raw, HEAD_BYTES as usize)
    } else {
        raw
    };
    text.split(|byte| *byte == b'\n')
        .take(max_lines)
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

/// Every transcript file of `source` under `root`, sorted by path.
pub(crate) fn discover(source: Source, root: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // Transcript trees are shallow; the bound keeps a symlink loop
                // or an unexpected tree from turning discovery into a crawl.
                if depth < 8 && !kind.is_symlink() {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() || !is_transcript(source, &path) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let compressed = path
                    .to_str()
                    .is_some_and(|name| name.ends_with(COMPRESSED_SUFFIX));
                out.push(Found {
                    path,
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    compressed,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn is_transcript(source: Source, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    // Codex compresses a rollout once its session is over.
    if !name.ends_with(".jsonl") && !name.ends_with(COMPRESSED_SUFFIX) {
        return false;
    }
    match source {
        // A workflow's `journal.jsonl` is the harness's bookkeeping, not a
        // transcript.
        Source::ClaudeCode => name != "journal.jsonl",
        Source::Codex => true,
    }
}

// ------------------------------------------------------------- the import

/// Everything an import was asked to do.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Which agent's transcripts.
    pub source: Source,
    /// Where they are.
    pub root: PathBuf,
    /// The tenant to stamp on spans (`None` leaves it to the destination: a
    /// bound credential stamps its own).
    pub tenant: Option<String>,
    /// Only files modified this recently.
    pub since: Option<Duration>,
    /// Only sessions whose file path or working directory contains this.
    pub project_filter: Option<String>,
    /// The state file, or `None` to send everything (`--no-state`).
    pub state: Option<PathBuf>,
    /// The state file's key for this destination.
    pub destination_key: String,
    /// Keep following the files.
    pub watch: bool,
    /// How often `--watch` polls.
    pub interval: Duration,
}

/// One file the import is following.
struct Tracked {
    /// Compressed files: the head digest of the plain file this one
    /// continues, which its decompressed content must start with for the
    /// plain file's offset to carry over.
    inherited_head: Option<(String, u64)>,
    /// Compressed files: the compressed size last recorded.
    stored_size: Option<u64>,
    converter: Option<Box<dyn Converter>>,
    /// Bytes consumed and sent (complete lines only).
    offset: u64,
    /// The run id recorded when the offset was last saved.
    run: Option<String>,
    /// When its converter was last fed.
    active: Instant,
}

/// Runs an import into `sink`, writing progress lines to `log`, and returns
/// what it did. With `options.watch` it never returns unless an error stops
/// it (or `stop` says so — tests use that; the CLI passes a function that
/// never stops).
pub fn run(
    options: &ImportOptions,
    sink: &mut dyn Sink,
    log: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> Result<ImportStats, String> {
    let mut stats = ImportStats {
        source: Some(options.source),
        ..ImportStats::default()
    };
    let mut state = match &options.state {
        Some(path) => state::State::load(path)?,
        None => state::State::default(),
    };
    let mut tracked: BTreeMap<PathBuf, Tracked> = BTreeMap::new();
    // Files `--project-filter` excluded: a session's working directory does
    // not change, so each file's head is read once, not once per poll.
    let mut excluded: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut sessions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut ancestry = codex::Ancestry::default();
    let mut first_pass = true;
    loop {
        let sent_before = stats.spans_sent;
        let found = discover(options.source, &options.root);
        if options.source == Source::Codex {
            ancestry.learn(&found);
        }
        // A file that vanished (Codex replaces a finished rollout with its
        // compressed copy) is no longer followed.
        if !tracked.is_empty() || !excluded.is_empty() {
            let present: std::collections::HashSet<&Path> =
                found.iter().map(|file| file.path.as_path()).collect();
            tracked.retain(|path, _| present.contains(path.as_path()));
            excluded.retain(|path| present.contains(path.as_path()));
        }
        let cutoff = options
            .since
            .and_then(|since| SystemTime::now().checked_sub(since));
        let mut changed_state = false;
        for file in &found {
            if first_pass {
                stats.files_seen += 1;
            }
            if let Some(cutoff) = cutoff {
                if file.modified < cutoff && !tracked.contains_key(&file.path) {
                    if first_pass {
                        stats.files_filtered += 1;
                    }
                    continue;
                }
            }
            if let Some(filter) = &options.project_filter {
                let verdict = if excluded.contains(&file.path) {
                    Some(false)
                } else if tracked.contains_key(&file.path) {
                    Some(true)
                } else {
                    project_matches(options.source, &file.path, filter)
                };
                if verdict != Some(true) {
                    if first_pass {
                        stats.files_filtered += 1;
                    }
                    // Undetermined (no working directory written yet) is
                    // asked again next poll; a definite no is remembered.
                    if verdict == Some(false) {
                        excluded.insert(file.path.clone());
                    }
                    continue;
                }
            }
            let key = file.path.to_string_lossy().into_owned();
            let entry = tracked.entry(file.path.clone()).or_insert_with(|| {
                let saved = state.file(&options.destination_key, &key);
                // A compressed rollout continues the plain file it replaced.
                let inherited = match (saved, file.compressed) {
                    (None, true) => key
                        .strip_suffix(".zst")
                        .and_then(|plain| state.file(&options.destination_key, plain)),
                    _ => None,
                };
                let known = saved.or(inherited);
                Tracked {
                    inherited_head: inherited.map(|plain| (plain.head.clone(), plain.head_len)),
                    stored_size: saved.and_then(|saved| saved.stored_size),
                    converter: None,
                    offset: known.map_or(0, |known| known.offset),
                    run: known.and_then(|known| known.run.clone()),
                    active: Instant::now(),
                }
            });
            if file.size == 0 {
                continue;
            }
            if file.compressed {
                // Compressed files are immutable once written: the same size
                // is the same file, and a different one is a rewrite.
                if entry.stored_size == Some(file.size) {
                    if first_pass {
                        stats.files_unchanged += 1;
                    }
                    continue;
                }
                if entry.stored_size.is_some() {
                    entry.offset = 0;
                    entry.run = None;
                }
                entry.converter = None;
            } else {
                if file.size == entry.offset && entry.offset > 0 {
                    if first_pass {
                        stats.files_unchanged += 1;
                    }
                    continue;
                }
                // A file that shrank or was rewritten is a new file. (Checked
                // only when the size moved, so an idle tree costs a stat per
                // file.)
                if file.size < entry.offset
                    || !state.head_matches(&options.destination_key, &key, &file.path)
                {
                    entry.offset = 0;
                    entry.converter = None;
                    entry.run = None;
                }
            }
            match follow(options, &ancestry, file, entry, sink, &mut stats) {
                // Unreadable (named in the errors): nothing is recorded, so
                // the next import tries again.
                Ok(Followed::Failed) => {}
                Ok(Followed::Read(run)) => {
                    if let Some(run) = &run {
                        sessions.insert(run.clone());
                    }
                    if file.compressed {
                        state.record_compressed(
                            &options.destination_key,
                            &key,
                            entry.offset,
                            run,
                            file.size,
                        );
                        entry.stored_size = Some(file.size);
                        entry.converter = None;
                    } else {
                        state.record(
                            &options.destination_key,
                            &key,
                            entry.offset,
                            run,
                            &file.path,
                        );
                    }
                    changed_state = true;
                }
                Err(Fatal(message)) => {
                    if let (Some(path), true) = (&options.state, changed_state) {
                        let _ = state.save(path);
                    }
                    return Err(message);
                }
            }
            if !options.watch {
                // A batch import holds one converter at a time.
                entry.converter = None;
            }
        }
        if changed_state {
            if let Some(path) = &options.state {
                if let Err(error) = state.save(path) {
                    stats.error(error);
                }
            }
        }
        stats.sessions = sessions.len() as u64;
        if first_pass {
            if options.watch {
                let _ = writeln!(log, "{}", summary_line(&stats, true));
            }
        } else if stats.spans_sent > sent_before {
            let _ = writeln!(
                log,
                "traza import {}: sent {} spans ({} in total)",
                options.source.as_str(),
                stats.spans_sent - sent_before,
                stats.spans_sent
            );
        }
        first_pass = false;
        if !options.watch || stop() {
            return Ok(stats);
        }
        evict(&mut tracked);
        std::thread::sleep(options.interval);
        if stop() {
            return Ok(stats);
        }
    }
}

/// A destination failure no retry or split can cure: stop the import.
struct Fatal(String);

/// What following one file came to.
enum Followed {
    /// Read and sent; the file's run id when known.
    Read(Option<String>),
    /// The file could not be read or decompressed.
    Failed,
}

/// Reads whatever `file` holds past what `entry` has consumed, converts it
/// and sends the spans it touched. Returns the file's run id when known.
fn follow(
    options: &ImportOptions,
    ancestry: &codex::Ancestry,
    file: &Found,
    entry: &mut Tracked,
    sink: &mut dyn Sink,
    stats: &mut ImportStats,
) -> Result<Followed, Fatal> {
    let tenant = options.tenant.clone().unwrap_or_default();
    // Resuming a live converter reads only the new bytes; anything else
    // re-reads from the start and suppresses what was already sent.
    let (start, floor, fresh) = match entry.converter {
        Some(_) => (entry.offset, 0, false),
        None => (0, entry.offset, true),
    };
    if fresh {
        let converter: Box<dyn Converter> = match options.source {
            Source::ClaudeCode => Box::new(claude_code::ClaudeCode::new(&file.path, &tenant)),
            Source::Codex => Box::new(codex::Codex::new(&file.path, &tenant, ancestry)),
        };
        entry.converter = Some(converter);
    }
    let Some(converter) = entry.converter.as_mut() else {
        return Ok(Followed::Failed);
    };
    let mut file_stats = FileStats::default();
    let mut floor = floor;
    let read = if file.compressed {
        decompressed(&file.path).and_then(|content| {
            // The plain file's offset carries over only if this is its
            // content: the same first bytes.
            if let Some((head, len)) = entry.inherited_head.take() {
                let same = content
                    .get(..len as usize)
                    .is_some_and(|prefix| crate::payload::sha256_hex(prefix) == head);
                if !same {
                    floor = 0;
                }
            }
            let consumed = read_lines_from(
                std::io::Cursor::new(&content[..]),
                0,
                converter.as_mut(),
                &mut file_stats,
            )?;
            // A compressed rollout is final. What its last turn held is
            // settled at a virtual line past its end, so it counts as new
            // against any floor the plain file left.
            converter.finish(consumed + 1, &mut file_stats);
            Ok(consumed)
        })
    } else {
        read_lines(&file.path, start, converter.as_mut(), &mut file_stats)
    };
    let consumed = match read {
        Ok(consumed) => consumed,
        Err(error) => {
            stats.files_failed += 1;
            stats.error(format!("{}: {error}", file.path.display()));
            entry.converter = None;
            return Ok(Followed::Failed);
        }
    };
    stats.files_read += 1;
    file_stats.capped = converter.spans().capped;
    let run = converter.run();
    // A run id that changed since the last save (a Codex parent thread that
    // appeared later) changes every id: everything is sent again.
    let floor = if entry.run.is_some() && run.is_some() && entry.run != run {
        0
    } else {
        floor
    };
    let spans = converter.spans().drain(floor);
    if !options.watch || fresh {
        stats.tally(converter.spans());
    }
    stats.transcript.merge(&file_stats);
    send_all(sink, spans, stats)?;
    entry.offset = consumed;
    entry.active = Instant::now();
    if run.is_some() {
        entry.run = run.clone();
    }
    Ok(Followed::Read(run))
}

/// Feeds every complete line of `path` from byte `start` into `converter`,
/// returning the offset after the last line consumed. An unterminated final
/// line is consumed only when it already parses as JSON (a writer that has
/// not yet written its newline); otherwise it waits for the next read.
fn read_lines(
    path: &Path,
    start: u64,
    converter: &mut dyn Converter,
    stats: &mut FileStats,
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    read_lines_from(
        BufReader::with_capacity(256 * 1024, file),
        start,
        converter,
        stats,
    )
}

/// A compressed transcript's whole content.
fn decompressed(path: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    crate::zstd::decompress(&raw, MAX_DECOMPRESSED_BYTES)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// [`read_lines`] over any reader positioned at byte `start`.
fn read_lines_from(
    mut reader: impl BufRead,
    start: u64,
    converter: &mut dyn Converter,
    stats: &mut FileStats,
) -> std::io::Result<u64> {
    let mut offset = start;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = read_bounded_line(&mut reader, &mut line)?;
        if read == 0 {
            return Ok(offset);
        }
        let terminated = line.last() == Some(&b'\n');
        let body: &[u8] = if terminated {
            &line[..line.len() - 1]
        } else {
            &line
        };
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if !terminated {
            // The tail: consume only a complete JSON value.
            if read as usize > MAX_LINE_BYTES
                || serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err()
            {
                return Ok(offset);
            }
        }
        let end = offset + read;
        if read as usize > MAX_LINE_BYTES {
            stats.lines += 1;
            stats.malformed_lines += 1;
        } else if !body.iter().all(u8::is_ascii_whitespace) {
            stats.lines += 1;
            converter.feed(body, end, stats);
        }
        offset = end;
    }
}

/// Reads one line into `out`, keeping at most [`MAX_LINE_BYTES`] + 1 bytes of
/// it (the rest is consumed and dropped). Returns the bytes consumed.
fn read_bounded_line(reader: &mut impl BufRead, out: &mut Vec<u8>) -> std::io::Result<u64> {
    let mut consumed = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(consumed);
        }
        let (chunk, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(at) => (&available[..=at], true),
            None => (available, false),
        };
        let take = chunk.len();
        if out.len() <= MAX_LINE_BYTES {
            let room = (MAX_LINE_BYTES + 1).saturating_sub(out.len());
            out.extend_from_slice(&chunk[..take.min(room)]);
            if done && take > room {
                // Keep the newline visible so the caller sees a terminated line.
                out.push(b'\n');
            }
        }
        reader.consume(take);
        consumed += take as u64;
        if done {
            return Ok(consumed);
        }
    }
}

/// Sends `spans` in bounded batches; a refused batch is split to isolate
/// the spans the destination will not take.
fn send_all(sink: &mut dyn Sink, spans: Vec<Span>, stats: &mut ImportStats) -> Result<(), Fatal> {
    let mut batch: Vec<Span> = Vec::new();
    let mut bytes = 0usize;
    for span in spans {
        let size = serde_json::to_vec(&span).map_or(0, |encoded| encoded.len());
        if !batch.is_empty() && (batch.len() >= BATCH_SPANS || bytes + size > BATCH_BYTES) {
            send_batch(sink, std::mem::take(&mut batch), stats)?;
            bytes = 0;
        }
        bytes += size;
        batch.push(span);
    }
    if !batch.is_empty() {
        send_batch(sink, batch, stats)?;
    }
    Ok(())
}

fn send_batch(sink: &mut dyn Sink, batch: Vec<Span>, stats: &mut ImportStats) -> Result<(), Fatal> {
    let count = batch.len() as u64;
    match sink.send(&batch) {
        Ok(()) => {
            stats.spans_sent += count;
            Ok(())
        }
        Err(sink::SendError::Fatal(message)) => Err(Fatal(message)),
        Err(sink::SendError::Refused(message)) if batch.len() == 1 => {
            stats.spans_refused += 1;
            stats.error(format!(
                "span {}/{} refused: {message}",
                batch[0].trace_id, batch[0].span_id
            ));
            Ok(())
        }
        Err(sink::SendError::Refused(_)) => {
            // Split in halves until the refused spans are isolated: a
            // batch is refused whole for one bad span.
            let mut left = batch;
            let right = left.split_off(left.len() / 2);
            send_batch(sink, left, stats)?;
            send_batch(sink, right, stats)
        }
    }
}

/// Drops converters idle past [`LIVE_IDLE`], and the least recently active
/// past [`MAX_LIVE_CONVERTERS`].
fn evict(tracked: &mut BTreeMap<PathBuf, Tracked>) {
    let now = Instant::now();
    for entry in tracked.values_mut() {
        if entry.converter.is_some() && now.duration_since(entry.active) > LIVE_IDLE {
            entry.converter = None;
        }
    }
    let mut live: Vec<(Instant, PathBuf)> = tracked
        .iter()
        .filter(|(_, entry)| entry.converter.is_some())
        .map(|(path, entry)| (entry.active, path.clone()))
        .collect();
    if live.len() > MAX_LIVE_CONVERTERS {
        live.sort();
        let excess = live.len() - MAX_LIVE_CONVERTERS;
        for (_, path) in live.into_iter().take(excess) {
            if let Some(entry) = tracked.get_mut(&path) {
                entry.converter = None;
            }
        }
    }
}

/// Whether a file belongs to a project matching `filter`: its path contains
/// it, or the working directory its first records name does. `None` when
/// the file names no working directory yet.
fn project_matches(source: Source, path: &Path, filter: &str) -> Option<bool> {
    if path.to_string_lossy().contains(filter) {
        return Some(true);
    }
    for line in head_lines(path, 64) {
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let cwd = match source {
            Source::ClaudeCode => str_field(&record, "cwd"),
            Source::Codex => record.get("payload").and_then(|p| str_field(p, "cwd")),
        };
        if let Some(cwd) = cwd {
            return Some(cwd.contains(filter));
        }
    }
    None
}

/// One human-readable line about an import.
pub fn summary_line(stats: &ImportStats, watching: bool) -> String {
    let unknown: u64 = stats.transcript.unknown_records.values().sum();
    format!(
        "traza import {}: {} files ({} read, {} unchanged, {} filtered, {} failed), {} sessions, \
         {} spans built ({} turns, {} llm, {} tool, {} agent), {} sent, {} refused; \
         {} lines, {} malformed, {} unknown records{}",
        stats.source.map_or("?", Source::as_str),
        stats.files_seen,
        stats.files_read,
        stats.files_unchanged,
        stats.files_filtered,
        stats.files_failed,
        stats.sessions,
        stats.spans_built,
        stats.turns,
        stats.llm_steps,
        stats.tool_steps,
        stats.agent_spans,
        stats.spans_sent,
        stats.spans_refused,
        stats.transcript.lines,
        stats.transcript.malformed_lines,
        unknown,
        if watching {
            "; watching for changes"
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod tests;
