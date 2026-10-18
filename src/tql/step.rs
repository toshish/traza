//! A run read as TQL steps, and the evaluation of atoms and predicates
//! against one step.
//!
//! A step is a span whose step kind is llm, tool or agent
//! ([`crate::semconv::StepKind::is_step`]), in the run's step order — the
//! same steps, in the same order and with the same indices, as the run's
//! tape (`GET /v1/tapes/{run}`). Everything a predicate reads is derived
//! here from the stored span; derived text (`args`, `result`), paths
//! (`file`), tool calls (`calls`) and the `edit`/`test` classifications are
//! computed on first use and kept for the rest of the match.
//!
//! Evaluation is bounded twice over: every text a predicate reads is cut to
//! [`MAX_TEXT_BYTES`], and every evaluation charges its work to the scan's
//! [`Meter`] and produces at most the environments its caller has room for
//! (dropping the rest, lowest priority first, and saying so).

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::ast::{Atom, AtomKind, Cond, Field, Literal, NameTest, Op, Pred, Test};
use super::meter::{Meter, BYTES_PER_UNIT, CONDITION_UNITS, FORK_UNITS, SPAN_UNITS, STATE_UNITS};
use super::regex::Regex;
use crate::semconv::{LlmFacts, StepFacts, StepKind};
use crate::{Deadline, Result, Span};

/// Bytes of any text a predicate sees — a step's input or output, a value
/// inside it, an attribute, a name, one entry of a list. Longer text is
/// matched on its first this-many bytes (stated in the guide).
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Most paths, tool calls or catalog entries kept per step.
const MAX_LIST: usize = 512;

/// Deepest JSON nesting walked when deriving text, paths and calls.
const MAX_JSON_DEPTH: usize = 32;

/// Hops followed up the parent chain to find a step's enclosing agent.
const MAX_ANCESTRY: usize = 64;

/// Spans whose facts are extracted between two deadline checks.
const DEADLINE_CHECK_SPANS: usize = 16;

/// Argument keys whose string value names a file.
pub(crate) const PATH_KEYS: [&str; 8] = [
    "file_path",
    "filePath",
    "path",
    "file",
    "filename",
    "target_file",
    "notebook_path",
    "absolute_path",
];

/// Tool names that edit a file (compared ignoring ASCII case).
pub(crate) const EDIT_TOOLS: [&str; 15] = [
    "edit",
    "multiedit",
    "write",
    "notebookedit",
    "apply_patch",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "edit_file",
    "write_file",
    "create_file",
    "replace_in_file",
    "write_to_file",
    "apply_diff",
    "search_replace",
    "insert_edit_into_file",
];

/// Argument keys that carry new file content.
pub(crate) const EDIT_KEYS: [&str; 10] = [
    "new_string",
    "new_str",
    "content",
    "contents",
    "file_text",
    "edits",
    "patch",
    "diff",
    "code_edit",
    "new_source",
];

/// Argument keys whose value is the command a tool step ran (a string, or
/// an array of strings joined by spaces).
pub(crate) const COMMAND_KEYS: [&str; 5] =
    ["command", "cmd", "commands", "script", "shell_command"];

/// What makes a tool step a test run: its command text runs a test runner
/// at a COMMAND POSITION — the start of a line, or after `&&`, `||`, `;`,
/// `|`, `(`, a backtick or `$(` — optionally behind environment assignments
/// (`CI=1`), wrappers (`npx`, `uv run`, `poetry run`, `bash -lc`, `env`,
/// `time`, …) and a directory (`.venv/bin/`, `./`). A runner named anywhere
/// else (`pip install pytest`, `cat pytest.ini`) is not a test run.
pub(crate) const TEST_RUNNER: &str = concat!(
    r#"(^|&&|\|\||[;|(`]|\$\()\s*"#,
    r#"([A-Za-z_][A-Za-z0-9_]*=[^\s;&|]*\s+)*"#,
    r#"((sudo|env|time|nice|exec|command|npx|bunx|uvx|yarn|pnpm|uv run|poetry run|pipenv run|pdm run|hatch run|rye run|npm exec|pnpm exec|yarn dlx|pnpm dlx|xvfb-run|timeout [0-9.]+[smhd]?|(ba|z|da)?sh( -[A-Za-z]+)*)\s+["']?([A-Za-z_][A-Za-z0-9_]*=[^\s;&|]*\s+)*)*"#,
    r#"([^\s;&|()`"']*/)?"#,
    r#"(pytest|py\.test|tox|nox|jest|vitest|mocha|rspec|phpunit|ctest|go test|cargo (test|nextest)|npm (run )?test|yarn (run )?test|pnpm (run )?test|bun test|deno test|python[0-9.]* -m (pytest|unittest)|mvn( [^\s;&|]+)* (test|verify)|gradlew?( [^\s;&|]+)*[ :](test|check)|make( [^\s;&|]+)* (test|check)|dotnet test|swift test|mix test|rake test|bundle exec (rspec|rake))\b"#,
);

fn test_runner() -> Option<&'static Regex> {
    static RUNNER: OnceLock<Option<Regex>> = OnceLock::new();
    RUNNER.get_or_init(|| Regex::new(TEST_RUNNER).ok()).as_ref()
}

/// One step of a run, as TQL reads it.
pub(crate) struct Step<'a> {
    /// Position among the run's steps, from zero.
    pub(crate) index: usize,
    pub(crate) span: &'a Span,
    pub(crate) facts: StepFacts,
    pub(crate) llm: LlmFacts,
    /// The agent the step ran under (see the `agent` field).
    pub(crate) agent: Option<String>,
    /// llm steps: their own tool catalog; tool steps: their requesting llm
    /// step's. `None` when unknown.
    pub(crate) catalog: Option<Rc<Catalog>>,
    args: OnceCell<Option<String>>,
    result: OnceCell<Option<String>>,
    command: OnceCell<Option<String>>,
    files: OnceCell<Vec<String>>,
    calls: OnceCell<Vec<String>>,
    edit: OnceCell<bool>,
    test: OnceCell<bool>,
    /// The request digest and fingerprint, computed on first use: the facts
    /// may come without them ([`crate::semconv::step_facts_undigested`]).
    digested: OnceCell<(Option<String>, String)>,
}

/// A model call's tool catalog.
#[derive(Debug)]
pub(crate) struct Catalog {
    /// The tool names offered, at most [`MAX_LIST`].
    pub(crate) names: Vec<String>,
    /// Whether every name fit under the cap. A truncated catalog can say
    /// what IS offered but not what is not.
    pub(crate) complete: bool,
    /// The names in ASCII lower case, so a lookup costs the name looked up
    /// rather than the whole catalog.
    lowered: HashSet<String>,
}

impl Catalog {
    fn new(names: Vec<String>, complete: bool) -> Self {
        let lowered = names.iter().map(|name| name.to_ascii_lowercase()).collect();
        Self {
            names,
            complete,
            lowered,
        }
    }

    /// Whether `name` is offered, ignoring ASCII case as TQL names do;
    /// `None` when the catalog was truncated and does not list it.
    pub(crate) fn offers(&self, name: &str) -> Option<bool> {
        if self.lowered.contains(&name.to_ascii_lowercase()) {
            return Some(true);
        }
        self.complete.then_some(false)
    }
}

impl<'a> Step<'a> {
    /// The tool, model or agent name a step's signature carries.
    pub(crate) fn name(&self) -> &str {
        let signature = self.facts.signature();
        signature
            .split_once(':')
            .map_or(signature, |(_, name)| name)
    }

    pub(crate) fn start_ns(&self) -> u64 {
        self.span.start_time_ns
    }

    /// The step's request digest and fingerprint, digested once.
    fn digested(&self) -> &(Option<String>, String) {
        self.digested.get_or_init(|| {
            crate::semconv::request_digest_and_fingerprint(self.span, &self.facts, &self.llm)
        })
    }

    fn args_text(&self) -> Option<&str> {
        self.args
            .get_or_init(|| self.facts.request.as_ref().map(value_text))
            .as_deref()
    }

    /// The command a tool step ran, for the `test` heuristic: the value of
    /// a [`COMMAND_KEYS`] key at the top of its arguments (an array of
    /// strings joined by spaces, several keys by newlines), or the
    /// arguments themselves when they are one string. File contents,
    /// descriptions and other argument text are not commands.
    fn command_text(&self) -> Option<&str> {
        self.command
            .get_or_init(|| {
                let text = match self.facts.request.as_ref()? {
                    Value::String(text) => text.clone(),
                    Value::Object(map) => {
                        let parts: Vec<String> = COMMAND_KEYS
                            .iter()
                            .filter_map(|key| match map.get(*key)? {
                                Value::String(text) => Some(text.clone()),
                                Value::Array(items) => Some(
                                    items
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                ),
                                _ => None,
                            })
                            .collect();
                        if parts.is_empty() {
                            return None;
                        }
                        parts.join("\n")
                    }
                    _ => return None,
                };
                Some(capped(&text).to_owned())
            })
            .as_deref()
    }

    fn result_text(&self) -> Option<&str> {
        self.result
            .get_or_init(|| self.facts.response.as_ref().map(value_text))
            .as_deref()
    }

    pub(crate) fn files(&self) -> &[String] {
        self.files
            .get_or_init(|| match (self.facts.kind, &self.facts.request) {
                (StepKind::Tool, Some(request)) => paths_in(request),
                _ => Vec::new(),
            })
    }

    fn calls(&self) -> &[String] {
        self.calls
            .get_or_init(|| match (self.facts.kind, &self.facts.response) {
                (StepKind::Llm, Some(response)) => calls_in(response),
                _ => Vec::new(),
            })
    }

    pub(crate) fn is_edit(&self) -> bool {
        *self.edit.get_or_init(|| {
            if self.facts.kind != StepKind::Tool || self.files().is_empty() {
                return false;
            }
            let Some(request) = &self.facts.request else {
                return false;
            };
            if let Some(command) = request.get("command").and_then(Value::as_str) {
                if matches!(command, "view" | "read") {
                    return false;
                }
            }
            let name = self.name();
            EDIT_TOOLS
                .iter()
                .any(|tool| tool.eq_ignore_ascii_case(name))
                || EDIT_KEYS.iter().any(|key| has_key(request, key, 0))
                || has_patch_marker(request)
        })
    }

    /// Whether the step runs a test suite: a tool step that is not an
    /// [edit](Self::is_edit) and whose [command](Self::command_text) runs a
    /// [`TEST_RUNNER`] at a command position. Its regex work is charged to
    /// `meter`; the answer is kept once known.
    pub(crate) fn is_test(&self, meter: &mut Meter) -> Result<bool> {
        if let Some(known) = self.test.get() {
            return Ok(*known);
        }
        let known = self.facts.kind == StepKind::Tool
            && !self.is_edit()
            && match (self.command_text(), test_runner()) {
                (Some(command), Some(runner)) => runner.is_match(command, meter)?,
                _ => false,
            };
        let _ = self.test.set(known);
        Ok(known)
    }

    fn tokens_reported(&self) -> bool {
        self.llm.prompt_tokens.is_some()
            || self.llm.completion_tokens.is_some()
            || self.llm.total_tokens.is_some()
    }

    /// A field's value on this step, every text in it cut to
    /// [`MAX_TEXT_BYTES`]. Deriving `test` runs a regex, charged to `meter`.
    pub(crate) fn field(&self, field: &Field, meter: &mut Meter) -> Result<Val<'_>> {
        Ok(match field {
            Field::Kind => Val::Text(Cow::Borrowed(self.facts.kind.as_str())),
            Field::Name => text(Some(self.name())),
            Field::Span => text(Some(&self.span.name)),
            Field::Service => text(Some(&self.span.service)),
            Field::Agent => text(self.agent.as_deref()),
            Field::Model => text(self.llm.model.as_deref()),
            Field::Tool => text(self.facts.tool.as_deref()),
            Field::Status => text(Some(&self.span.status)),
            Field::Error => Val::Bool(self.span.status.eq_ignore_ascii_case("error")),
            Field::Cost => self.llm.cost_usd.map_or(Val::Missing, Val::Num),
            Field::Duration => Val::Num(
                self.span
                    .end_time_ns
                    .saturating_sub(self.span.start_time_ns) as f64,
            ),
            Field::Tokens => {
                if self.tokens_reported() {
                    Val::Num(self.llm.total() as f64)
                } else {
                    Val::Missing
                }
            }
            Field::InputTokens => number(self.llm.prompt_tokens),
            Field::OutputTokens => number(self.llm.completion_tokens),
            Field::CacheRead => number(self.llm.cache_read_tokens),
            Field::CacheWrite => number(self.llm.cache_creation_tokens),
            Field::Context => number(self.llm.context_tokens()),
            Field::Index => Val::Num(self.index as f64),
            Field::Args => text(self.args_text()),
            Field::Result => text(self.result_text()),
            Field::ArgsPath(path) => self
                .facts
                .request
                .as_ref()
                .and_then(|value| navigate(value, path))
                .map_or(Val::Missing, json_val),
            Field::ResultPath(path) => self
                .facts
                .response
                .as_ref()
                .and_then(|value| navigate(value, path))
                .map_or(Val::Missing, json_val),
            Field::File => Val::List(Cow::Borrowed(self.files())),
            Field::Digest => text(self.digested().0.as_deref()),
            Field::Fingerprint => text(Some(&self.digested().1)),
            Field::Signature => text(Some(self.facts.signature())),
            Field::Calls => match self.facts.kind {
                StepKind::Llm => Val::List(Cow::Borrowed(self.calls())),
                _ => Val::Missing,
            },
            Field::Offered => self.catalog.as_ref().map_or(Val::Missing, |catalog| {
                Val::List(Cow::Borrowed(catalog.names.as_slice()))
            }),
            Field::InCatalog => match (self.facts.kind, &self.catalog) {
                (StepKind::Tool, Some(catalog)) => {
                    catalog.offers(self.name()).map_or(Val::Missing, Val::Bool)
                }
                _ => Val::Missing,
            },
            Field::UnknownCalls => match (self.facts.kind, &self.catalog) {
                (StepKind::Llm, Some(catalog)) if catalog.complete => {
                    let calls = self.calls();
                    meter.charge(
                        calls
                            .iter()
                            .map(|call| CONDITION_UNITS + call.len() as u64 / BYTES_PER_UNIT)
                            .sum(),
                    )?;
                    Val::List(Cow::Owned(
                        calls
                            .iter()
                            .filter(|call| catalog.offers(call) == Some(false))
                            .cloned()
                            .collect(),
                    ))
                }
                _ => Val::Missing,
            },
            Field::Edit => Val::Bool(self.is_edit()),
            Field::TestRun => Val::Bool(self.is_test(meter)?),
            Field::Attr(key) => self.span.attributes.get(key).map_or(Val::Missing, json_val),
        })
    }
}

/// `text` cut to its first [`MAX_TEXT_BYTES`] bytes, at a character
/// boundary.
pub(crate) fn capped(text: &str) -> &str {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn text(value: Option<&str>) -> Val<'_> {
    value.map_or(Val::Missing, |text| Val::Text(Cow::Borrowed(capped(text))))
}

fn number(value: Option<u64>) -> Val<'static> {
    value.map_or(Val::Missing, |n| Val::Num(n as f64))
}

/// A field's value.
#[derive(Clone, Debug)]
pub(crate) enum Val<'s> {
    Missing,
    Text(Cow<'s, str>),
    Num(f64),
    Bool(bool),
    List(Cow<'s, [String]>),
}

fn json_val(value: &Value) -> Val<'_> {
    match value {
        Value::Null => Val::Missing,
        Value::String(text) => Val::Text(Cow::Borrowed(capped(text))),
        Value::Number(number) => number.as_f64().map_or(Val::Missing, Val::Num),
        Value::Bool(flag) => Val::Bool(*flag),
        Value::Array(items) => Val::List(Cow::Owned(
            items
                .iter()
                .take(MAX_LIST)
                .map(|item| match item {
                    Value::String(text) => capped(text).to_owned(),
                    other => bounded_json(other),
                })
                .collect(),
        )),
        Value::Object(_) => Val::Text(Cow::Owned(bounded_json(value))),
    }
}

/// `value`'s JSON text, cut to its first [`MAX_TEXT_BYTES`] bytes at a
/// character boundary. Serialization stops at the cap, so a huge structure
/// costs no more than a small one.
fn bounded_json(value: &Value) -> String {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let room = MAX_TEXT_BYTES - self.0.len();
            if room == 0 {
                return Err(std::io::Error::other("text cap reached"));
            }
            let take = buf.len().min(room);
            self.0.extend_from_slice(&buf[..take]);
            Ok(take)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded(Vec::new());
    // An error here is the cap being reached: what was written is kept.
    let _ = serde_json::to_writer(&mut out, value);
    let mut bytes = out.0;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) => error.valid_up_to(),
    };
    bytes.truncate(valid);
    String::from_utf8(bytes).unwrap_or_default()
}

/// `value` at a dotted path: object keys, and array indices for arrays.
fn navigate<'v>(value: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter()
        .try_fold(value, |current, segment| match current {
            Value::Object(map) => map.get(segment),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index)),
            _ => None,
        })
}

/// The text a predicate matches for a stored value: a string as itself, a
/// scalar as its JSON text, and a structure as its string leaves in
/// document order (object keys sorted), one per line — capped at
/// [`MAX_TEXT_BYTES`].
pub(crate) fn value_text(value: &Value) -> String {
    fn walk(value: &Value, depth: usize, out: &mut String) {
        if out.len() >= MAX_TEXT_BYTES || depth > MAX_JSON_DEPTH {
            return;
        }
        match value {
            Value::String(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, depth + 1, out)),
            Value::Object(map) => map.values().for_each(|item| walk(item, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = match value {
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => {
            let mut out = String::new();
            walk(value, 0, &mut out);
            out
        }
        other => other.to_string(),
    };
    if out.len() > MAX_TEXT_BYTES {
        let mut end = MAX_TEXT_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// A list of distinct texts, in first-seen order, at most [`MAX_LIST`].
/// Membership is a hash lookup, so filling it from a long input costs that
/// input once — not once per entry already kept.
#[derive(Default)]
struct Unique {
    list: Vec<String>,
    seen: HashSet<String>,
}

impl Unique {
    fn len(&self) -> usize {
        self.list.len()
    }

    /// Adds `item` (trimmed, cut to [`MAX_TEXT_BYTES`]) unless empty or
    /// already listed; `false` when the list was full and a new item was
    /// dropped.
    fn push(&mut self, item: &str) -> bool {
        let item = capped(item.trim());
        if item.is_empty() || self.seen.contains(item) {
            return true;
        }
        if self.list.len() >= MAX_LIST {
            return false;
        }
        self.seen.insert(item.to_owned());
        self.list.push(item.to_owned());
        true
    }
}

/// Paths a tool step's arguments name: [`PATH_KEYS`] anywhere in the
/// arguments, and the file headers of a patch (`*** Update File: …`,
/// `*** Add File: …`, `*** Delete File: …`, `*** Move to: …`, `+++ b/…`).
pub(crate) fn paths_in(request: &Value) -> Vec<String> {
    fn walk(value: &Value, depth: usize, out: &mut Unique) {
        if depth > MAX_JSON_DEPTH || out.len() >= MAX_LIST {
            return;
        }
        match value {
            Value::Object(map) => {
                for key in PATH_KEYS {
                    if let Some(Value::String(path)) = map.get(key) {
                        out.push(path);
                    }
                }
                map.values().for_each(|item| walk(item, depth + 1, out));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, depth + 1, out)),
            Value::String(text) => patch_paths(text, out),
            _ => {}
        }
    }
    let mut out = Unique::default();
    walk(request, 0, &mut out);
    out.list
}

const PATCH_HEADERS: [&str; 5] = [
    "*** Update File: ",
    "*** Add File: ",
    "*** Delete File: ",
    "*** Move to: ",
    "+++ b/",
];

fn patch_paths(text: &str, out: &mut Unique) {
    if !text.contains("*** ") && !text.contains("+++ b/") {
        return;
    }
    for line in text.lines() {
        for header in PATCH_HEADERS {
            if let Some(path) = line.strip_prefix(header) {
                out.push(path);
            }
        }
    }
}

fn has_patch_marker(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("*** Begin Patch") || text.contains("\n+++ b/"),
        Value::Array(items) => items.iter().any(has_patch_marker),
        Value::Object(map) => map.values().any(has_patch_marker),
        _ => false,
    }
}

fn has_key(value: &Value, key: &str, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|item| has_key(item, key, depth + 1))
        }
        Value::Array(items) => items.iter().any(|item| has_key(item, key, depth + 1)),
        _ => false,
    }
}

/// Tool names an llm response asked for: entries of a `tool_calls` list
/// (`name` or `function.name`), content blocks typed `tool_call`,
/// `tool_use` or `function_call`, and a legacy `function_call` object.
pub(crate) fn calls_in(response: &Value) -> Vec<String> {
    fn call_name(value: &Value) -> Option<&str> {
        value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("function")?.get("name")?.as_str())
    }
    fn walk(value: &Value, depth: usize, out: &mut Unique) {
        if depth > MAX_JSON_DEPTH || out.len() >= MAX_LIST {
            return;
        }
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(calls)) = map.get("tool_calls") {
                    for call in calls {
                        if let Some(name) = call_name(call) {
                            out.push(name);
                        }
                    }
                }
                if let Some(kind) = map.get("type").and_then(Value::as_str) {
                    if matches!(kind, "tool_call" | "tool_use" | "function_call") {
                        if let Some(name) = map.get("name").and_then(Value::as_str) {
                            out.push(name);
                        }
                    }
                }
                if let Some(call) = map.get("function_call").filter(|call| call.is_object()) {
                    if let Some(name) = call_name(call) {
                        out.push(name);
                    }
                }
                map.values().for_each(|item| walk(item, depth + 1, out));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Unique::default();
    walk(response, 0, &mut out);
    out.list
}

/// The tool catalog an llm span offered: `gen_ai.tool.definitions`
/// (OpenTelemetry GenAI; a list, or its JSON text, of `{name}` or
/// `{function: {name}}`), OpenLLMetry's `llm.request.functions.{i}.name`,
/// and OpenInference's `llm.tools.{i}.tool.json_schema`. `None` when the
/// span records none of them; `complete: false` when more names than
/// [`MAX_LIST`] were offered.
///
/// The work — text parsed, entries and attributes visited, names kept — is
/// charged to `meter` as it is done, so a span carrying a huge catalog
/// cannot hold a request past its deadline. Reading stops at the first name
/// the full list has to drop: nothing after it can change the answer.
pub(crate) fn catalog_of(
    attributes: &Map<String, Value>,
    meter: &mut Meter,
) -> Result<Option<Catalog>> {
    fn entry_name(value: &Value) -> Option<&str> {
        value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("function")?.get("name")?.as_str())
    }
    /// `value`, or the JSON its text holds (charging the text), borrowed
    /// when it is structured already.
    fn structured<'v>(value: &'v Value, meter: &mut Meter) -> Result<Cow<'v, Value>> {
        if let Value::String(text) = value {
            meter.charge(text.len() as u64 / BYTES_PER_UNIT + 1)?;
            let trimmed = text.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    return Ok(Cow::Owned(parsed));
                }
            }
        }
        Ok(Cow::Borrowed(value))
    }
    /// Adds `name`, charging it; `false` once the full list drops one.
    fn keep(names: &mut Unique, name: &str, meter: &mut Meter) -> Result<bool> {
        meter.charge(CONDITION_UNITS + name.len() as u64 / BYTES_PER_UNIT)?;
        Ok(names.push(name))
    }
    let mut names = Unique::default();
    let mut complete = true;
    let mut found = false;
    if let Some(definitions) = attributes.get("gen_ai.tool.definitions") {
        let parsed = structured(definitions, meter)?;
        if let Value::Array(entries) = &*parsed {
            found = true;
            for entry in entries {
                meter.charge(CONDITION_UNITS)?;
                if let Some(name) = entry_name(entry) {
                    if !keep(&mut names, name, meter)? {
                        complete = false;
                        break;
                    }
                }
            }
        }
    }
    let mut schemas: Vec<(u64, Cow<'_, Value>)> = Vec::new();
    let mut indexed: Vec<(u64, &str)> = Vec::new();
    for (key, value) in attributes {
        meter.charge(CONDITION_UNITS + key.len() as u64 / BYTES_PER_UNIT)?;
        if let Some(rest) = key.strip_prefix("llm.request.functions.") {
            if let Some((index, "name")) = rest.split_once('.') {
                if let (Ok(index), Some(name)) = (index.parse::<u64>(), value.as_str()) {
                    indexed.push((index, name));
                }
            }
        } else if let Some(rest) = key.strip_prefix("llm.tools.") {
            if let Some((index, "tool.json_schema")) = rest.split_once('.') {
                if let Ok(index) = index.parse::<u64>() {
                    let schema = structured(value, meter)?;
                    if entry_name(&schema).is_some() {
                        schemas.push((index, schema));
                    }
                }
            }
        }
    }
    indexed.extend(
        schemas
            .iter()
            .filter_map(|(index, schema)| Some((*index, entry_name(schema)?))),
    );
    if !indexed.is_empty() {
        found = true;
        meter.charge(indexed.len() as u64)?;
        indexed.sort_unstable();
        if complete {
            for (_, name) in indexed {
                if !keep(&mut names, name, meter)? {
                    complete = false;
                    break;
                }
            }
        }
    }
    Ok(found.then(|| Catalog::new(names.list, complete)))
}

/// Reads a run's spans (in step order) as steps. `facts` extracts a span's
/// step and LLM facts — the store's priced extraction in production.
///
/// Every loop over the spans answers to `deadline`: the facts are read with
/// a check every [`DEADLINE_CHECK_SPANS`] spans, and the rest — the span
/// index, ancestry, catalogs — charges a meter that checks it in proportion
/// to the work.
/// The deadline is consulted once more before the steps are returned, so a
/// run whose reading ran past it is not passed off as read.
pub(crate) fn build_steps<'a>(
    spans: &'a [Span],
    mut facts: impl FnMut(&Span) -> (StepFacts, LlmFacts),
    deadline: Option<Deadline>,
) -> Result<Vec<Step<'a>>> {
    let mut extracted = Vec::with_capacity(spans.len());
    for (examined, span) in spans.iter().enumerate() {
        if examined % DEADLINE_CHECK_SPANS == 0 {
            Deadline::check(deadline, 0)?;
        }
        extracted.push(facts(span));
    }
    let mut meter = Meter::new(deadline);
    let mut by_id: HashMap<(&str, &str), usize> = HashMap::with_capacity(spans.len());
    let mut agents: Vec<Option<String>> = Vec::with_capacity(spans.len());
    for (position, (span, (facts, _))) in spans.iter().zip(&extracted).enumerate() {
        meter.charge(STATE_UNITS)?;
        by_id
            .entry((span.trace_id.as_str(), span.span_id.as_str()))
            .or_insert(position);
        agents.push(facts.agent.clone());
    }
    let enclosing_agent = |position: usize, meter: &mut Meter| -> Result<Option<String>> {
        let mut current = position;
        for _ in 0..MAX_ANCESTRY {
            meter.charge(STATE_UNITS)?;
            let span = &spans[current];
            let Some(parent) = span.parent_span_id.as_deref() else {
                return Ok(None);
            };
            let Some(&found) = by_id.get(&(span.trace_id.as_str(), parent)) else {
                return Ok(None);
            };
            current = found;
            if let Some(agent) = &agents[current] {
                return Ok(Some(agent.clone()));
            }
        }
        Ok(None)
    };
    let mut steps: Vec<Step<'a>> = Vec::new();
    let mut llm_catalogs: HashMap<usize, Option<Rc<Catalog>>> = HashMap::new();
    // The latest model call's catalog under each agent (`None`: no agent).
    let mut latest_llm: HashMap<Option<String>, Option<Rc<Catalog>>> = HashMap::new();
    for (position, (span, (step_facts, llm))) in spans.iter().zip(extracted).enumerate() {
        meter.charge(SPAN_UNITS)?;
        if !step_facts.kind.is_step() {
            continue;
        }
        let agent = match &step_facts.agent {
            Some(agent) => Some(agent.clone()),
            None => enclosing_agent(position, &mut meter)?,
        };
        let catalog = match step_facts.kind {
            StepKind::Llm => {
                let catalog = catalog_of(&span.attributes, &mut meter)?.map(Rc::new);
                llm_catalogs.insert(position, catalog.clone());
                latest_llm.insert(agent.clone(), catalog.clone());
                catalog
            }
            StepKind::Tool => {
                // The requesting model call: the tool span's parent when that
                // is a model call, else the latest model call before it under
                // the same agent — never another agent's, whose tools may
                // differ.
                let parent = span
                    .parent_span_id
                    .as_deref()
                    .and_then(|parent| by_id.get(&(span.trace_id.as_str(), parent)))
                    .and_then(|position| llm_catalogs.get(position));
                match parent {
                    Some(catalog) => catalog.clone(),
                    None => latest_llm.get(&agent).cloned().flatten(),
                }
            }
            _ => None,
        };
        steps.push(Step {
            index: steps.len(),
            span,
            facts: step_facts,
            llm,
            agent,
            catalog,
            args: OnceCell::new(),
            result: OnceCell::new(),
            command: OnceCell::new(),
            files: OnceCell::new(),
            calls: OnceCell::new(),
            edit: OnceCell::new(),
            test: OnceCell::new(),
            digested: OnceCell::new(),
        });
    }
    Deadline::check(deadline, 0)?;
    Ok(steps)
}

// --------------------------------------------------------------- captures

/// A captured value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Captured {
    Str(Rc<str>),
    /// An `f64`'s bits (normalized so `-0.0` equals `0.0`).
    Num(u64),
}

impl Captured {
    fn number(value: f64) -> Self {
        Self::Num(if value == 0.0 { 0.0_f64 } else { value }.to_bits())
    }

    pub(crate) fn as_number(&self) -> Option<f64> {
        match self {
            Self::Num(bits) => Some(f64::from_bits(*bits)),
            Self::Str(text) => text.trim().parse().ok(),
        }
    }

    /// The value as JSON, a text cut to its first `max_bytes` bytes (at a
    /// character boundary), and whether it was cut.
    pub(crate) fn to_json_capped(&self, max_bytes: usize) -> (Value, bool) {
        match self {
            Self::Str(text) => {
                if text.len() <= max_bytes {
                    return (Value::String(text.to_string()), false);
                }
                let mut end = max_bytes;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                (Value::String(text[..end].to_owned()), true)
            }
            Self::Num(bits) => (
                serde_json::Number::from_f64(f64::from_bits(*bits))
                    .map_or(Value::Null, Value::Number),
                false,
            ),
        }
    }
}

/// The captures bound so far along one path through the pattern.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Env(Rc<Vec<Option<Captured>>>);

impl Env {
    pub(crate) fn new(slots: usize) -> Self {
        Self(Rc::new(vec![None; slots]))
    }

    pub(crate) fn get(&self, slot: usize) -> Option<&Captured> {
        self.0.get(slot).and_then(Option::as_ref)
    }

    fn with(&self, slot: usize, value: Captured) -> Self {
        let mut slots = (*self.0).clone();
        slots[slot] = Some(value);
        Self(Rc::new(slots))
    }

    pub(crate) fn slots(&self) -> &[Option<Captured>] {
        &self.0
    }

    /// This environment with `slot` bound to the text `value`.
    #[cfg(test)]
    pub(crate) fn with_slot_for_tests(&self, slot: usize, value: &str) -> Self {
        self.with(slot, Captured::Str(Rc::from(value)))
    }

    /// Bytes hashing or comparing this environment reads — what a
    /// remembered state costs.
    pub(crate) fn weight(&self) -> u64 {
        self.0
            .iter()
            .map(|slot| match slot {
                Some(Captured::Str(text)) => text.len() as u64,
                _ => 8,
            })
            .sum()
    }
}

/// The values a field offers a binding, in order: one per distinct element
/// of a list, one for a scalar, none when missing.
fn bindable(value: &Val<'_>) -> Vec<Captured> {
    match value {
        Val::Missing => Vec::new(),
        Val::Text(text) => vec![Captured::Str(Rc::from(text.as_ref()))],
        Val::Num(number) => vec![Captured::number(*number)],
        Val::Bool(flag) => vec![Captured::Str(Rc::from(if *flag {
            "true"
        } else {
            "false"
        }))],
        Val::List(items) => {
            let mut seen: HashSet<&str> = HashSet::with_capacity(items.len());
            items
                .iter()
                .filter(|item| seen.insert(item.as_str()))
                .map(|item| Captured::Str(Rc::from(item.as_str())))
                .collect()
        }
    }
}

/// Whether `value` offers `captured` — `bindable(value)` contains it —
/// without building the candidates.
fn offers(value: &Val<'_>, captured: &Captured) -> bool {
    match (value, captured) {
        (Val::Text(text), Captured::Str(bound)) => text.as_ref() == bound.as_ref(),
        (Val::Num(number), Captured::Num(_)) => Captured::number(*number) == *captured,
        (Val::Bool(flag), Captured::Str(bound)) => {
            bound.as_ref() == if *flag { "true" } else { "false" }
        }
        (Val::List(items), Captured::Str(bound)) => {
            items.iter().any(|item| item.as_str() == bound.as_ref())
        }
        _ => false,
    }
}

/// Units reading `value` costs, beyond a condition's base.
fn weight(value: &Val<'_>) -> u64 {
    match value {
        Val::Text(text) => text.len() as u64 / BYTES_PER_UNIT,
        Val::List(items) => items.len() as u64 * 4,
        _ => 0,
    }
}

// -------------------------------------------------------------- evaluation

/// Pushes `env` unless `out` already holds `limit` environments; then the
/// alternative is dropped, and the meter records that a cap was hit.
fn push_capped(out: &mut Vec<Env>, env: Env, limit: usize, meter: &mut Meter) {
    if out.len() < limit {
        out.push(env);
    } else {
        meter.dropped = true;
    }
}

/// A comparison's right-hand side, resolved against the bindings.
enum Target<'l> {
    Str(Cow<'l, str>),
    Num(f64),
    Bool(bool),
    Regex(&'l Regex),
}

fn resolve<'l>(literal: &'l Literal, env: &Env) -> Option<Target<'l>> {
    Some(match literal {
        Literal::Str(text) => Target::Str(Cow::Borrowed(text)),
        Literal::Num(number) => Target::Num(*number),
        Literal::Duration(nanos) => Target::Num(*nanos as f64),
        Literal::Bool(flag) => Target::Bool(*flag),
        Literal::Regex(regex) => Target::Regex(regex),
        Literal::Capture { slot, factor } => {
            let captured = env.get(*slot)?;
            match factor {
                Some(factor) => Target::Num(captured.as_number()? * factor),
                None => match captured {
                    Captured::Str(text) => Target::Str(Cow::Owned(text.to_string())),
                    Captured::Num(bits) => Target::Num(f64::from_bits(*bits)),
                },
            }
        }
    })
}

fn scalar_equals(value: &str, target: &Target<'_>) -> bool {
    match target {
        Target::Str(text) => value == text.as_ref(),
        Target::Num(number) => value
            .trim()
            .parse::<f64>()
            .is_ok_and(|parsed| parsed == *number),
        Target::Bool(flag) => value.eq_ignore_ascii_case(if *flag { "true" } else { "false" }),
        Target::Regex(_) => false,
    }
}

fn equals(value: &Val<'_>, target: &Target<'_>) -> bool {
    match value {
        Val::Missing => false,
        Val::Text(text) => scalar_equals(text, target),
        Val::Num(number) => match target {
            Target::Num(other) => number == other,
            Target::Str(text) => text
                .trim()
                .parse::<f64>()
                .is_ok_and(|parsed| parsed == *number),
            _ => false,
        },
        Val::Bool(flag) => match target {
            Target::Bool(other) => flag == other,
            Target::Str(text) => text.eq_ignore_ascii_case(if *flag { "true" } else { "false" }),
            _ => false,
        },
        Val::List(items) => items.iter().any(|item| scalar_equals(item, target)),
    }
}

fn regex_matches(value: &Val<'_>, regex: &Regex, meter: &mut Meter) -> Result<bool> {
    Ok(match value {
        Val::Missing => false,
        Val::Text(text) => regex.is_match(text, meter)?,
        Val::Num(number) => regex.is_match(&number.to_string(), meter)?,
        Val::Bool(flag) => regex.is_match(if *flag { "true" } else { "false" }, meter)?,
        Val::List(items) => {
            for item in items.iter() {
                if regex.is_match(item, meter)? {
                    return Ok(true);
                }
            }
            false
        }
    })
}

fn ordered(value: &Val<'_>, op: Op, target: f64) -> bool {
    let holds = |number: f64| match op {
        Op::Gt => number > target,
        Op::Ge => number >= target,
        Op::Lt => number < target,
        Op::Le => number <= target,
        _ => false,
    };
    match value {
        Val::Num(number) => holds(*number),
        Val::Text(text) => text.trim().parse::<f64>().is_ok_and(holds),
        Val::List(items) => items
            .iter()
            .any(|item| item.trim().parse::<f64>().is_ok_and(holds)),
        Val::Missing | Val::Bool(_) => false,
    }
}

fn truthy(value: &Val<'_>) -> bool {
    match value {
        Val::Missing => false,
        Val::Text(text) => !text.is_empty(),
        Val::Num(number) => *number != 0.0,
        Val::Bool(flag) => *flag,
        Val::List(items) => !items.is_empty(),
    }
}

/// Whether `op literal` holds for `value` under `env`. A capture that is
/// not bound resolves to nothing: every comparison with it is false, and so
/// `!=` and `!~` (their negations) are true.
fn compare(
    value: &Val<'_>,
    op: Op,
    literal: &Literal,
    env: &Env,
    meter: &mut Meter,
) -> Result<bool> {
    let target = resolve(literal, env);
    Ok(match op {
        Op::Eq => target.is_some_and(|target| equals(value, &target)),
        Op::Ne => !target.is_some_and(|target| equals(value, &target)),
        Op::Match | Op::NotMatch => {
            let hit = match &target {
                Some(Target::Regex(regex)) => regex_matches(value, regex, meter)?,
                _ => false,
            };
            hit == (op == Op::Match)
        }
        Op::Gt | Op::Ge | Op::Lt | Op::Le => match target {
            Some(Target::Num(number)) => ordered(value, op, number),
            _ => false,
        },
    })
}

/// A predicate as a plain test: nothing binds (a binding position whose
/// capture is unbound is false).
pub(crate) fn test_pred(
    pred: &Pred,
    step: &Step<'_>,
    env: &Env,
    meter: &mut Meter,
) -> Result<bool> {
    match pred {
        Pred::And(items) => {
            for item in items {
                if !test_pred(item, step, env, meter)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Pred::Or(items) => {
            for item in items {
                if test_pred(item, step, env, meter)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Pred::Not(inner) => Ok(!test_pred(inner, step, env, meter)?),
        Pred::Cond(cond) => {
            let value = step.field(&cond.field, meter)?;
            meter.charge(CONDITION_UNITS + weight(&value))?;
            Ok(match &cond.test {
                Test::Truthy => truthy(&value),
                Test::Compare(op, literal) => compare(&value, *op, literal, env, meter)?,
                Test::Bind(slot) => env
                    .get(*slot)
                    .is_some_and(|captured| offers(&value, captured)),
            })
        }
    }
}

/// A binding position over `value` for a path holding `env`: the path
/// itself when the capture is bound to one of the values, else one fork per
/// value, in order — at most until `out` holds `limit`.
fn bind(
    value: &Val<'_>,
    slot: usize,
    env: Env,
    out: &mut Vec<Env>,
    limit: usize,
    meter: &mut Meter,
) -> Result<()> {
    match env.get(slot) {
        Some(bound) => {
            meter.charge(CONDITION_UNITS + weight(value))?;
            if offers(value, bound) {
                push_capped(out, env, limit, meter);
            }
        }
        None => {
            if value_is_missing(value) {
                return Ok(());
            }
            if out.len() >= limit {
                meter.dropped = true;
                return Ok(());
            }
            meter.charge(CONDITION_UNITS + weight(value))?;
            for candidate in bindable(value) {
                if out.len() >= limit {
                    meter.dropped = true;
                    break;
                }
                meter.charge(FORK_UNITS)?;
                out.push(env.with(slot, candidate));
            }
        }
    }
    Ok(())
}

fn value_is_missing(value: &Val<'_>) -> bool {
    match value {
        Val::Missing => true,
        Val::List(items) => items.is_empty(),
        _ => false,
    }
}

/// A predicate evaluated with bindings: pushes the environments it holds
/// under onto `out`, in priority order, while `out` holds fewer than
/// `limit` (past that, alternatives are dropped and the meter says so).
/// Binding positions fork one environment per value of a multi-valued
/// field, so a chain of them multiplies — which is why every intermediate
/// list is held to `limit` too.
fn eval_pred(
    pred: &Pred,
    step: &Step<'_>,
    env: Env,
    out: &mut Vec<Env>,
    limit: usize,
    meter: &mut Meter,
) -> Result<()> {
    match pred {
        Pred::And(items) => {
            let mut current = vec![env];
            for item in items {
                let mut next = Vec::new();
                match item {
                    // A binding's values are the same for every path: read
                    // the field once.
                    Pred::Cond(Cond {
                        field,
                        test: Test::Bind(slot),
                    }) => {
                        let value = step.field(field, meter)?;
                        for env in current {
                            bind(&value, *slot, env, &mut next, limit, meter)?;
                        }
                    }
                    _ => {
                        for env in current {
                            eval_pred(item, step, env, &mut next, limit, meter)?;
                        }
                    }
                }
                if next.is_empty() {
                    return Ok(());
                }
                current = next;
            }
            for env in current {
                push_capped(out, env, limit, meter);
            }
        }
        Pred::Cond(Cond {
            field,
            test: Test::Bind(slot),
        }) => {
            let value = step.field(field, meter)?;
            bind(&value, *slot, env, out, limit, meter)?;
        }
        other => {
            if test_pred(other, step, &env, meter)? {
                push_capped(out, env, limit, meter);
            }
        }
    }
    Ok(())
}

/// Whether a step is of the atom's kind.
fn kind_matches(kind: AtomKind, step: &Step<'_>, meter: &mut Meter) -> Result<bool> {
    Ok(match kind {
        AtomKind::Step => true,
        AtomKind::Tool => step.facts.kind == StepKind::Tool,
        AtomKind::Llm => step.facts.kind == StepKind::Llm,
        AtomKind::Agent => step.facts.kind == StepKind::Agent,
        AtomKind::Edit => step.is_edit(),
        AtomKind::Test => step.is_test(meter)?,
    })
}

/// The values an atom's names are tested against: the files of an `edit`
/// atom, the step's name otherwise.
fn name_values<'s>(kind: AtomKind, step: &'s Step<'_>) -> Vec<&'s str> {
    match kind {
        AtomKind::Edit => step.files().iter().map(String::as_str).collect(),
        _ => vec![capped(step.name())],
    }
}

/// An atom evaluated against one step: pushes the environments it holds
/// under onto `out`, in priority order, while `out` holds fewer than
/// `limit`; nothing when the step does not satisfy it. Its work is charged
/// to `meter`, which also records any alternative the limit dropped.
pub(crate) fn eval_atom(
    atom: &Atom,
    step: &Step<'_>,
    env: &Env,
    out: &mut Vec<Env>,
    limit: usize,
    meter: &mut Meter,
) -> Result<()> {
    if !kind_matches(atom.kind, step, meter)? {
        return Ok(());
    }
    let mut current = vec![env.clone()];
    if !atom.names.is_empty() {
        let values = name_values(atom.kind, step);
        current = match atom.names.as_slice() {
            [NameTest::Capture { slot, bind: true }] => match env.get(*slot) {
                Some(Captured::Str(bound)) => {
                    if values.iter().any(|value| *value == bound.as_ref()) {
                        current
                    } else {
                        return Ok(());
                    }
                }
                Some(Captured::Num(_)) => return Ok(()),
                None => {
                    let mut forks: Vec<Env> = Vec::new();
                    let mut seen: HashSet<&str> = HashSet::new();
                    for value in values {
                        if !seen.insert(value) {
                            continue;
                        }
                        if forks.len() >= limit {
                            meter.dropped = true;
                            break;
                        }
                        meter.charge(FORK_UNITS)?;
                        forks.push(env.with(*slot, Captured::Str(Rc::from(value))));
                    }
                    forks
                }
            },
            names => {
                let mut hit = false;
                'names: for name in names {
                    for value in &values {
                        let matched = match name {
                            NameTest::Exact(expected) => value.eq_ignore_ascii_case(expected),
                            NameTest::Regex(regex) => regex.is_match(value, meter)?,
                            NameTest::Capture { slot, .. } => {
                                matches!(env.get(*slot), Some(Captured::Str(bound)) if bound.as_ref() == *value)
                            }
                        };
                        if matched {
                            hit = true;
                            break 'names;
                        }
                    }
                }
                if !hit {
                    return Ok(());
                }
                current
            }
        };
    }
    for pred in &atom.preds {
        let mut next = Vec::new();
        for env in current {
            eval_pred(pred, step, env, &mut next, limit, meter)?;
        }
        if next.is_empty() {
            return Ok(());
        }
        current = next;
    }
    for env in current {
        push_capped(out, env, limit, meter);
    }
    Ok(())
}

/// Whether an atom reads any capture (so its result depends on the
/// environment and cannot be shared across threads at one position).
pub(crate) fn atom_uses_captures(atom: &Atom) -> bool {
    atom.names
        .iter()
        .any(|name| matches!(name, NameTest::Capture { .. }))
        || atom.preds.iter().any(pred_uses_captures)
}

pub(crate) fn pred_uses_captures(pred: &Pred) -> bool {
    match pred {
        Pred::And(items) | Pred::Or(items) => items.iter().any(pred_uses_captures),
        Pred::Not(inner) => pred_uses_captures(inner),
        Pred::Cond(cond) => matches!(
            cond.test,
            Test::Bind(_) | Test::Compare(_, Literal::Capture { .. })
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A span for unit tests: `kind` is `llm`, `tool` or `agent`.
    pub(crate) fn step_span(
        index: usize,
        kind: &str,
        name: &str,
        status: &str,
        extra: Value,
    ) -> Span {
        let mut attributes = Map::new();
        match kind {
            "llm" => {
                attributes.insert("gen_ai.operation.name".into(), json!("chat"));
                attributes.insert("gen_ai.request.model".into(), json!(name));
            }
            "tool" => {
                attributes.insert("gen_ai.operation.name".into(), json!("execute_tool"));
                attributes.insert("gen_ai.tool.name".into(), json!(name));
            }
            _ => {
                attributes.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
                attributes.insert("gen_ai.agent.name".into(), json!(name));
            }
        }
        if let Value::Object(extra) = extra {
            attributes.extend(extra);
        }
        Span {
            trace_id: "t".into(),
            span_id: format!("s{index:04}"),
            tenant: String::new(),
            parent_span_id: None,
            name: format!("op-{index}"),
            start_time_ns: 1_000 + index as u64 * 1_000_000_000,
            end_time_ns: 1_000 + index as u64 * 1_000_000_000 + 500_000_000,
            status: status.into(),
            service: "svc".into(),
            attributes,
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    pub(crate) fn unpriced(span: &Span) -> (StepFacts, LlmFacts) {
        let llm = crate::semconv::facts(&span.attributes);
        (crate::semconv::step_facts_with(span, &llm), llm)
    }

    pub(crate) fn steps(spans: &[Span]) -> Vec<Step<'_>> {
        build_steps(spans, unpriced, None).expect("builds")
    }

    pub(crate) fn is_test(step: &Step<'_>) -> bool {
        step.is_test(&mut Meter::new(None)).expect("no deadline")
    }

    #[test]
    fn digests_are_computed_only_for_the_steps_a_pattern_asks_about() {
        let spans = vec![
            bash(0, json!("cargo test")),
            step_span(
                1,
                "llm",
                "gpt-4o",
                "ok",
                json!({"gen_ai.input.messages": [{"role": "user", "content": "x".repeat(4096)}]}),
            ),
        ];
        let undigested = |span: &Span| {
            let llm = crate::semconv::facts(&span.attributes);
            (crate::semconv::step_facts_undigested(span, &llm), llm)
        };
        let lazy = build_steps(&spans, undigested, None).expect("builds");
        let eager = steps(&spans);
        for (lazy, eager) in lazy.iter().zip(&eager) {
            // Nothing is digested by building the steps, nor by reading a
            // field that is not a digest.
            assert!(lazy.facts.request_digest.is_none());
            assert_eq!(
                field_of(lazy, &Field::Signature),
                field_of(eager, &Field::Signature)
            );
            assert!(lazy.digested.get().is_none());
            // Asked for, the digest and fingerprint are the eager ones.
            assert!(eager.facts.request_digest.is_some());
            assert_eq!(
                field_of(lazy, &Field::Digest),
                field_of(eager, &Field::Digest)
            );
            assert_eq!(
                field_of(lazy, &Field::Fingerprint),
                field_of(eager, &Field::Fingerprint)
            );
            assert!(lazy.digested.get().is_some());
        }
    }

    fn field_of(step: &Step<'_>, field: &Field) -> String {
        format!(
            "{:?}",
            step.field(field, &mut Meter::new(None))
                .expect("no deadline")
        )
    }

    fn bash(index: usize, command: Value) -> Span {
        step_span(
            index,
            "tool",
            "Bash",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"command": command, "description": "pytest the code"}}),
        )
    }

    #[test]
    fn a_test_run_is_a_runner_at_a_command_position() {
        let runs = [
            json!("pytest -q"),
            json!("cd repo && pytest tests/test_a.py"),
            json!("uv run pytest -x"),
            json!("poetry run pytest"),
            json!("CI=1 npm test"),
            json!("npx jest --ci"),
            json!("./gradlew :app:test"),
            json!("python3 -m pytest"),
            json!(".venv/bin/pytest -q"),
            json!("go test ./..."),
            json!("make -j8 check"),
            json!("mvn -q verify"),
            json!("git stash; cargo test --lib"),
            json!("bash -lc \"pytest -q\""),
            // Codex records the argv as an array.
            json!(["bash", "-lc", "cargo nextest run"]),
        ];
        let not_runs = [
            json!("pip install pytest-xdist"),
            json!("cat pytest.ini"),
            json!("grep -r pytest ."),
            json!("echo run the tests"),
            json!("npm install jest"),
            json!("make -C tests"),
            json!("mvn -Dmaven.test.skip=true install"),
            json!("ls tests/"),
        ];
        let mut spans: Vec<Span> = runs
            .iter()
            .chain(not_runs.iter())
            .enumerate()
            .map(|(index, command)| bash(index, command.clone()))
            .collect();
        // An edit whose new content mentions a runner is an edit, not a run.
        spans.push(step_span(
            spans.len(),
            "tool",
            "Edit",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"file_path": "a.py", "old_string": "x",
                   "new_string": "import pytest\npytest -q\n"}}),
        ));
        // A runner in a non-command argument does not count.
        spans.push(step_span(
            spans.len(),
            "tool",
            "Search",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"query": "pytest -q"}}),
        ));
        // A patch applied through the shell is an edit, even when a context
        // line of the patched script starts with a runner.
        spans.push(step_span(
            spans.len(),
            "tool",
            "shell",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"command": ["apply_patch",
                   "*** Begin Patch\n*** Update File: ci.sh\n@@\n pytest -q\n+echo done\n*** End Patch"]}}),
        ));
        // Arguments that are one string are the command.
        spans.push(step_span(
            spans.len(),
            "tool",
            "shell",
            "ok",
            json!({"gen_ai.tool.call.arguments": "pytest -k login"}),
        ));
        let steps = steps(&spans);
        let flagged: Vec<usize> = steps
            .iter()
            .filter(|step| is_test(step))
            .map(|step| step.index)
            .collect();
        let mut expected: Vec<usize> = (0..runs.len()).collect();
        expected.push(spans.len() - 1);
        assert_eq!(flagged, expected);
    }

    #[test]
    fn catalogs_compare_names_like_tql_and_never_guess() {
        let names: Vec<Value> = (0..MAX_LIST + 10)
            .map(|i| json!({"name": format!("tool_{i}")}))
            .collect();
        let mut planner = step_span(0, "agent", "planner", "ok", json!({}));
        planner.span_id = "p".into();
        let mut coder = step_span(1, "agent", "coder", "ok", json!({}));
        coder.span_id = "c".into();
        let mut planner_llm = step_span(
            2,
            "llm",
            "m",
            "ok",
            json!({"gen_ai.tool.definitions": [{"name": "Bash"}],
                   "gen_ai.output.messages": [{"role": "assistant", "parts": [
                       {"type": "tool_call", "name": "bash"}, {"type": "tool_call", "name": "Deploy"}]}]}),
        );
        planner_llm.parent_span_id = Some("p".into());
        let mut lower = step_span(3, "tool", "bash", "ok", json!({}));
        lower.parent_span_id = Some("p".into());
        // The coder's tool runs after the planner's model call, under
        // another agent: the planner's catalog says nothing about it.
        let mut other_agent = step_span(4, "tool", "Teleport", "ok", json!({}));
        other_agent.parent_span_id = Some("c".into());
        let big_llm = step_span(
            5,
            "llm",
            "m",
            "ok",
            json!({"gen_ai.tool.definitions": names}),
        );
        let listed = step_span(6, "tool", "tool_3", "ok", json!({}));
        let past_cap = step_span(
            7,
            "tool",
            &format!("tool_{}", MAX_LIST + 5),
            "ok",
            json!({}),
        );
        let spans = vec![
            planner,
            coder,
            planner_llm,
            lower,
            other_agent,
            big_llm,
            listed,
            past_cap,
        ];
        let steps = steps(&spans);
        assert_eq!(field_of(&steps[3], &Field::InCatalog), "Bool(true)", "case");
        assert_eq!(
            field_of(&steps[2], &Field::UnknownCalls),
            "List([\"Deploy\"])",
            "a call differing only in case is offered"
        );
        assert_eq!(field_of(&steps[4], &Field::InCatalog), "Missing");
        assert_eq!(field_of(&steps[6], &Field::InCatalog), "Bool(true)");
        assert_eq!(
            field_of(&steps[7], &Field::InCatalog),
            "Missing",
            "a truncated catalog cannot say a tool is absent"
        );
        assert_eq!(field_of(&steps[5], &Field::UnknownCalls), "Missing");
    }

    #[test]
    fn every_text_a_predicate_reads_is_capped() {
        let long = "x".repeat(MAX_TEXT_BYTES * 2);
        let spans = vec![step_span(
            0,
            "tool",
            "Bash",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"big": long, "list": [long, {"k": long}],
                   "object": {"k": long}},
                   "custom.attr": long}),
        )];
        let steps = steps(&spans);
        let mut meter = Meter::new(None);
        for field in [
            Field::ArgsPath(vec!["big".into()]),
            Field::ArgsPath(vec!["object".into()]),
            Field::Attr("custom.attr".into()),
            Field::Args,
        ] {
            match steps[0].field(&field, &mut meter).expect("no deadline") {
                Val::Text(text) => assert!(text.len() <= MAX_TEXT_BYTES, "{field:?}"),
                other => panic!("{field:?}: {other:?}"),
            }
        }
        match steps[0]
            .field(&Field::ArgsPath(vec!["list".into()]), &mut meter)
            .expect("no deadline")
        {
            Val::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(items.iter().all(|item| item.len() <= MAX_TEXT_BYTES));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(bounded_json(&json!({"a": "é"})), "{\"a\":\"é\"}");
    }

    #[test]
    fn list_bindings_fork_at_most_the_limit_and_say_so() {
        let names: Vec<Value> = (0..64).map(|i| json!({"name": format!("t{i}")})).collect();
        let spans = vec![step_span(
            0,
            "llm",
            "m",
            "ok",
            json!({"gen_ai.tool.definitions": names}),
        )];
        let steps = steps(&spans);
        // Every intermediate list is held to the limit: three bindings
        // (262,144 combinations, small enough to survive if they were all
        // built) fork about 8,000 environments, not all of them. Checked
        // first, so an unbounded chain fails here, cleanly.
        let three = super::super::Query::compile("llm[offered=@a, offered=@b, offered=@c]")
            .expect("compiles");
        let super::super::ast::Node::Atom(three) = &three.node else {
            panic!("one atom");
        };
        let mut meter = Meter::new(None);
        let mut out = Vec::new();
        eval_atom(three, &steps[0], &Env::new(3), &mut out, 4096, &mut meter).expect("no deadline");
        assert_eq!(out.len(), 4096);
        assert!(meter.dropped);
        assert!(meter.spent() < 4_000_000, "spent {}", meter.spent());

        let query = super::super::Query::compile(
            "llm[offered=@a, offered=@b, offered=@c, offered=@d, offered=@e]",
        )
        .expect("compiles");
        let super::super::ast::Node::Atom(atom) = &query.node else {
            panic!("one atom");
        };
        let mut meter = Meter::new(None);
        let mut out = Vec::new();
        eval_atom(atom, &steps[0], &Env::new(5), &mut out, 4096, &mut meter).expect("no deadline");
        // 64^5 combinations; the limit keeps the first 4,096 in priority
        // order and records the drop.
        assert_eq!(out.len(), 4096);
        assert!(meter.dropped);
        assert_eq!(out[0].get(4), Some(&Captured::Str(Rc::from("t0"))));
        assert_eq!(out[1].get(4), Some(&Captured::Str(Rc::from("t1"))));
        // The work is bounded by the limit, not by the combinations.
        assert!(meter.spent() < 20_000_000, "spent {}", meter.spent());

        // The fan-out consults the deadline as it goes.
        let expired =
            Deadline::starting(std::time::Instant::now(), Some(std::time::Duration::ZERO));
        let mut meter = Meter::new(expired);
        match eval_atom(
            atom,
            &steps[0],
            &Env::new(5),
            &mut Vec::new(),
            4096,
            &mut meter,
        ) {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
    }

    #[test]
    fn derived_fields_read_edits_tests_paths_and_calls() {
        let spans = vec![
            step_span(
                0,
                "llm",
                "claude",
                "ok",
                json!({
                    "gen_ai.tool.definitions": [{"name": "Bash"}, {"type": "function", "function": {"name": "Edit"}}],
                    "gen_ai.output.messages": [{"role": "assistant", "parts": [
                        {"type": "tool_call", "name": "Bash", "arguments": {"command": "pytest -q"}},
                        {"type": "tool_call", "name": "Deploy"}
                    ]}],
                    "gen_ai.usage.input_tokens": 1200,
                }),
            ),
            step_span(
                1,
                "tool",
                "Bash",
                "error",
                json!({"gen_ai.tool.call.arguments": {"command": "cd repo && pytest tests/test_a.py", "description": "run"},
                       "gen_ai.tool.call.result": "1 failed, 2 passed"}),
            ),
            step_span(
                2,
                "tool",
                "Edit",
                "ok",
                json!({"gen_ai.tool.call.arguments": {"file_path": "tests/test_a.py", "old_string": "a", "new_string": "b"}}),
            ),
            step_span(
                3,
                "tool",
                "Read",
                "ok",
                json!({"gen_ai.tool.call.arguments": {"file_path": "src/a.py"}}),
            ),
            step_span(
                4,
                "tool",
                "apply_patch",
                "ok",
                json!({"gen_ai.tool.call.arguments": {"input": "*** Begin Patch\n*** Update File: src/a.py\n@@\n-x\n+y\n*** Add File: src/b.py\n*** End Patch"}}),
            ),
            step_span(5, "tool", "Deploy", "ok", json!({})),
        ];
        let steps = steps(&spans);
        assert_eq!(steps.len(), 6);
        let field = |index: usize, field: Field| {
            format!(
                "{:?}",
                steps[index]
                    .field(&field, &mut Meter::new(None))
                    .expect("no deadline")
            )
        };
        assert!(
            field(0, Field::Calls).contains("Bash") && field(0, Field::Calls).contains("Deploy")
        );
        assert!(field(0, Field::UnknownCalls).contains("Deploy"));
        assert!(!field(0, Field::UnknownCalls).contains("Bash"));
        assert_eq!(field(0, Field::Context), "Num(1200.0)");
        assert!(is_test(&steps[1]), "pytest after && is a test run");
        assert!(!is_test(&steps[0]), "an llm step is never a test run");
        assert!(steps[2].is_edit());
        assert_eq!(steps[2].files(), ["tests/test_a.py"]);
        assert!(
            !steps[3].is_edit(),
            "a read names a file without editing it"
        );
        assert_eq!(steps[3].files(), ["src/a.py"]);
        assert!(steps[4].is_edit());
        assert_eq!(steps[4].files(), ["src/a.py", "src/b.py"]);
        // Tool steps inherit the latest model call's catalog.
        assert_eq!(field(1, Field::InCatalog), "Bool(true)");
        assert_eq!(field(5, Field::InCatalog), "Bool(false)");
        assert_eq!(field(1, Field::Error), "Bool(true)");
        assert!(field(1, Field::Args).contains("pytest tests/test_a.py"));
        assert_eq!(
            field(1, Field::ArgsPath(vec!["description".into()])),
            "Text(\"run\")"
        );
        assert_eq!(field(1, Field::Duration), "Num(500000000.0)");
    }

    #[test]
    fn a_step_runs_under_its_nearest_enclosing_agent() {
        let mut planner = step_span(0, "agent", "planner", "ok", json!({}));
        planner.span_id = "p".into();
        let mut wrapper = step_span(1, "tool", "x", "ok", json!({}));
        wrapper.attributes.clear();
        wrapper.span_id = "w".into();
        wrapper.parent_span_id = Some("p".into());
        let mut call = step_span(2, "llm", "m", "ok", json!({}));
        call.parent_span_id = Some("w".into());
        let orphan = step_span(3, "llm", "m", "ok", json!({}));
        let spans = vec![planner, wrapper, call, orphan];
        let steps = steps(&spans);
        // The wrapper is not a step; the call two levels down runs under
        // the planner, and a span with no agent ancestry under none.
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1].agent.as_deref(), Some("planner"));
        assert_eq!(steps[2].agent, None);
    }

    #[test]
    fn catalogs_from_every_convention() {
        let attributes = json!({
            "llm.request.functions.1.name": "b",
            "llm.request.functions.0.name": "a",
            "llm.tools.2.tool.json_schema": "{\"type\":\"function\",\"function\":{\"name\":\"c\"}}",
        });
        let names = |attributes: &Value| {
            catalog_of(
                attributes.as_object().expect("object"),
                &mut Meter::new(None),
            )
            .expect("no deadline")
            .map(|catalog| catalog.names)
        };
        assert_eq!(
            names(&attributes),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        let text = json!({"gen_ai.tool.definitions": "[{\"name\": \"x\"}]"});
        assert_eq!(names(&text), Some(vec!["x".into()]));
        assert_eq!(names(&json!({})), None);
    }

    #[test]
    fn a_deadline_spent_after_the_facts_stops_the_catalogs_too() {
        // The deadline expires while the last span's facts are read, so the
        // facts loop's own checks have all passed; the catalogs built after
        // it must still notice before the run is reported as read.
        let catalog: Vec<Value> = (0..400).map(|i| json!({"name": format!("t{i}")})).collect();
        let spans = vec![
            step_span(
                0,
                "llm",
                "m",
                "ok",
                json!({"gen_ai.tool.definitions": catalog}),
            ),
            step_span(1, "tool", "t3", "ok", json!({})),
        ];
        let deadline = crate::Deadline::starting(
            std::time::Instant::now(),
            Some(std::time::Duration::from_millis(200)),
        );
        let mut read = 0;
        let outcome = build_steps(
            &spans,
            |span| {
                read += 1;
                if read == spans.len() {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
                unpriced(span)
            },
            deadline,
        );
        match outcome {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            Ok(steps) => panic!("{} steps reported after the deadline", steps.len()),
            Err(other) => panic!("expected a deadline refusal, got {other:?}"),
        }
    }

    #[test]
    fn reading_a_catalog_is_charged_and_stops_once_its_answer_is_known() {
        // Many entries repeating few names: every entry is charged, and
        // each costs a lookup rather than a pass over the names kept.
        let repeated: Vec<Value> = (0..20_000)
            .map(|i| json!({"name": format!("t{}", i % 500)}))
            .collect();
        let attributes = json!({"gen_ai.tool.definitions": repeated});
        let mut meter = Meter::new(None);
        let catalog = catalog_of(attributes.as_object().expect("object"), &mut meter)
            .expect("no deadline")
            .expect("a catalog");
        assert!(catalog.complete);
        assert_eq!(catalog.names.len(), 500);
        assert!(
            meter.spent() >= 20_000 * CONDITION_UNITS,
            "{}",
            meter.spent()
        );
        assert_eq!(catalog.offers("T7"), Some(true), "case is ignored");
        assert_eq!(catalog.offers("t500"), Some(false));

        // More distinct names than fit: reading stops at the first dropped.
        let distinct: Vec<Value> = (0..20_000)
            .map(|i| json!({"name": format!("t{i}")}))
            .collect();
        let attributes = json!({"gen_ai.tool.definitions": distinct});
        let mut stopped = Meter::new(None);
        let catalog = catalog_of(attributes.as_object().expect("object"), &mut stopped)
            .expect("no deadline")
            .expect("a catalog");
        assert!(!catalog.complete);
        assert_eq!(catalog.names.len(), MAX_LIST);
        assert_eq!(catalog.offers("t9999"), None, "a cut catalog cannot say no");
        assert!(stopped.spent() < meter.spent() / 10, "{}", stopped.spent());

        // A spent deadline stops a long catalog partway.
        let expired =
            crate::Deadline::starting(std::time::Instant::now(), Some(std::time::Duration::ZERO));
        let attributes = json!({"gen_ai.tool.definitions": repeated_names(200_000)});
        match catalog_of(
            attributes.as_object().expect("object"),
            &mut Meter::new(expired),
        ) {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
    }

    fn repeated_names(entries: usize) -> Vec<Value> {
        (0..entries).map(|_| json!({"name": "Bash"})).collect()
    }

    #[test]
    fn text_is_capped_and_leaves_join_with_newlines() {
        assert_eq!(
            value_text(&json!({"b": "two", "a": ["one", 3]})),
            "one\ntwo"
        );
        assert_eq!(value_text(&json!(42)), "42");
        let long = "é".repeat(MAX_TEXT_BYTES);
        let text = value_text(&json!(long));
        assert!(text.len() <= MAX_TEXT_BYTES && text.is_char_boundary(text.len()));
    }
}
