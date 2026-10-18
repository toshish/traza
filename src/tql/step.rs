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

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::ast::{Atom, AtomKind, Cond, Field, Literal, NameTest, Op, Pred, Test};
use super::regex::Regex;
use crate::semconv::{LlmFacts, StepFacts, StepKind};
use crate::{Deadline, Result, Span};

/// Bytes of a step's input or output text a predicate sees. Longer text is
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

/// What makes a tool step a test run: its input text matches this.
pub(crate) const TEST_RUNNER: &str = r"(^|[\s;&|(/])(pytest|py\.test|tox|nox|jest|vitest|mocha|rspec|phpunit|ctest|go test|cargo (test|nextest)|npm (run )?test|yarn (run )?test|pnpm (run )?test|bun test|deno test|python[0-9.]* -m (pytest|unittest)|mvn [^\n]*test|gradlew? [^\n]*test|make [^\n]*(test|check)|dotnet test|swift test|mix test|rake test|bundle exec (rspec|rake))\b";

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
    pub(crate) catalog: Option<Rc<Vec<String>>>,
    args: OnceCell<Option<String>>,
    result: OnceCell<Option<String>>,
    files: OnceCell<Vec<String>>,
    calls: OnceCell<Vec<String>>,
    edit: OnceCell<bool>,
    test: OnceCell<bool>,
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

    fn args_text(&self) -> Option<&str> {
        self.args
            .get_or_init(|| self.facts.request.as_ref().map(value_text))
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

    pub(crate) fn is_test(&self) -> bool {
        *self.test.get_or_init(|| {
            self.facts.kind == StepKind::Tool
                && self
                    .args_text()
                    .zip(test_runner())
                    .is_some_and(|(text, runner)| runner.is_match(text))
        })
    }

    fn tokens_reported(&self) -> bool {
        self.llm.prompt_tokens.is_some()
            || self.llm.completion_tokens.is_some()
            || self.llm.total_tokens.is_some()
    }

    /// A field's value on this step.
    pub(crate) fn field(&self, field: &Field) -> Val<'_> {
        match field {
            Field::Kind => Val::Text(Cow::Borrowed(self.facts.kind.as_str())),
            Field::Name => Val::Text(Cow::Borrowed(self.name())),
            Field::Span => Val::Text(Cow::Borrowed(&self.span.name)),
            Field::Service => Val::Text(Cow::Borrowed(&self.span.service)),
            Field::Agent => text(self.agent.as_deref()),
            Field::Model => text(self.llm.model.as_deref()),
            Field::Tool => text(self.facts.tool.as_deref()),
            Field::Status => Val::Text(Cow::Borrowed(&self.span.status)),
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
            Field::Digest => text(self.facts.request_digest.as_deref()),
            Field::Fingerprint => Val::Text(Cow::Borrowed(&self.facts.fingerprint)),
            Field::Signature => Val::Text(Cow::Borrowed(self.facts.signature())),
            Field::Calls => match self.facts.kind {
                StepKind::Llm => Val::List(Cow::Borrowed(self.calls())),
                _ => Val::Missing,
            },
            Field::Offered => self.catalog.as_ref().map_or(Val::Missing, |catalog| {
                Val::List(Cow::Borrowed(catalog.as_slice()))
            }),
            Field::InCatalog => match (self.facts.kind, &self.catalog) {
                (StepKind::Tool, Some(catalog)) => {
                    let name = self.name();
                    Val::Bool(catalog.iter().any(|offered| offered == name))
                }
                _ => Val::Missing,
            },
            Field::UnknownCalls => match (self.facts.kind, &self.catalog) {
                (StepKind::Llm, Some(catalog)) => Val::List(Cow::Owned(
                    self.calls()
                        .iter()
                        .filter(|call| !catalog.contains(call))
                        .cloned()
                        .collect(),
                )),
                _ => Val::Missing,
            },
            Field::Edit => Val::Bool(self.is_edit()),
            Field::TestRun => Val::Bool(self.is_test()),
            Field::Attr(key) => self.span.attributes.get(key).map_or(Val::Missing, json_val),
        }
    }
}

fn text(value: Option<&str>) -> Val<'_> {
    value.map_or(Val::Missing, |text| Val::Text(Cow::Borrowed(text)))
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
        Value::String(text) => Val::Text(Cow::Borrowed(text)),
        Value::Number(number) => number.as_f64().map_or(Val::Missing, Val::Num),
        Value::Bool(flag) => Val::Bool(*flag),
        Value::Array(items) => Val::List(Cow::Owned(
            items
                .iter()
                .take(MAX_LIST)
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect(),
        )),
        Value::Object(_) => Val::Text(Cow::Owned(value.to_string())),
    }
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

fn push_unique(list: &mut Vec<String>, item: &str) {
    let item = item.trim();
    if !item.is_empty() && list.len() < MAX_LIST && !list.iter().any(|known| known == item) {
        list.push(item.to_owned());
    }
}

/// Paths a tool step's arguments name: [`PATH_KEYS`] anywhere in the
/// arguments, and the file headers of a patch (`*** Update File: …`,
/// `*** Add File: …`, `*** Delete File: …`, `*** Move to: …`, `+++ b/…`).
pub(crate) fn paths_in(request: &Value) -> Vec<String> {
    fn walk(value: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > MAX_JSON_DEPTH || out.len() >= MAX_LIST {
            return;
        }
        match value {
            Value::Object(map) => {
                for key in PATH_KEYS {
                    if let Some(Value::String(path)) = map.get(key) {
                        push_unique(out, path);
                    }
                }
                map.values().for_each(|item| walk(item, depth + 1, out));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, depth + 1, out)),
            Value::String(text) => patch_paths(text, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(request, 0, &mut out);
    out
}

const PATCH_HEADERS: [&str; 5] = [
    "*** Update File: ",
    "*** Add File: ",
    "*** Delete File: ",
    "*** Move to: ",
    "+++ b/",
];

fn patch_paths(text: &str, out: &mut Vec<String>) {
    if !text.contains("*** ") && !text.contains("+++ b/") {
        return;
    }
    for line in text.lines() {
        for header in PATCH_HEADERS {
            if let Some(path) = line.strip_prefix(header) {
                push_unique(out, path);
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
    fn walk(value: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > MAX_JSON_DEPTH || out.len() >= MAX_LIST {
            return;
        }
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(calls)) = map.get("tool_calls") {
                    for call in calls {
                        if let Some(name) = call_name(call) {
                            push_unique(out, name);
                        }
                    }
                }
                if let Some(kind) = map.get("type").and_then(Value::as_str) {
                    if matches!(kind, "tool_call" | "tool_use" | "function_call") {
                        if let Some(name) = map.get("name").and_then(Value::as_str) {
                            push_unique(out, name);
                        }
                    }
                }
                if let Some(call) = map.get("function_call").filter(|call| call.is_object()) {
                    if let Some(name) = call_name(call) {
                        push_unique(out, name);
                    }
                }
                map.values().for_each(|item| walk(item, depth + 1, out));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(response, 0, &mut out);
    out
}

/// The tool catalog an llm span offered: `gen_ai.tool.definitions`
/// (OpenTelemetry GenAI; a list, or its JSON text, of `{name}` or
/// `{function: {name}}`), OpenLLMetry's `llm.request.functions.{i}.name`,
/// and OpenInference's `llm.tools.{i}.tool.json_schema`. `None` when the
/// span records none of them.
pub(crate) fn catalog_of(attributes: &Map<String, Value>) -> Option<Vec<String>> {
    fn entry_name(value: &Value) -> Option<&str> {
        value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("function")?.get("name")?.as_str())
    }
    let mut names = Vec::new();
    let mut found = false;
    if let Some(definitions) = attributes.get("gen_ai.tool.definitions") {
        let parsed = crate::semconv::field_value(definitions);
        if let Value::Array(entries) = parsed {
            found = true;
            for entry in &entries {
                if let Some(name) = entry_name(entry) {
                    push_unique(&mut names, name);
                }
            }
        }
    }
    let mut indexed: Vec<(u64, String)> = Vec::new();
    for (key, value) in attributes {
        if let Some(rest) = key.strip_prefix("llm.request.functions.") {
            if let Some((index, "name")) = rest.split_once('.') {
                if let (Ok(index), Some(name)) = (index.parse::<u64>(), value.as_str()) {
                    indexed.push((index, name.to_owned()));
                }
            }
        } else if let Some(rest) = key.strip_prefix("llm.tools.") {
            if let Some((index, "tool.json_schema")) = rest.split_once('.') {
                let schema = crate::semconv::field_value(value);
                if let (Ok(index), Some(name)) = (index.parse::<u64>(), entry_name(&schema)) {
                    indexed.push((index, name.to_owned()));
                }
            }
        }
    }
    if !indexed.is_empty() {
        found = true;
        indexed.sort();
        for (_, name) in indexed {
            push_unique(&mut names, &name);
        }
    }
    found.then_some(names)
}

/// Reads a run's spans (in step order) as steps. `facts` extracts a span's
/// step and LLM facts — the store's priced extraction in production.
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
    let mut by_id: HashMap<(&str, &str), usize> = HashMap::with_capacity(spans.len());
    for (position, span) in spans.iter().enumerate() {
        by_id
            .entry((span.trace_id.as_str(), span.span_id.as_str()))
            .or_insert(position);
    }
    let agents: Vec<Option<String>> = extracted
        .iter()
        .map(|(facts, _)| facts.agent.clone())
        .collect();
    let enclosing_agent = |position: usize| -> Option<String> {
        let mut current = position;
        for _ in 0..MAX_ANCESTRY {
            let span = &spans[current];
            let parent = span.parent_span_id.as_deref()?;
            current = *by_id.get(&(span.trace_id.as_str(), parent))?;
            if let Some(agent) = &agents[current] {
                return Some(agent.clone());
            }
        }
        None
    };
    let mut steps: Vec<Step<'a>> = Vec::new();
    let mut llm_catalogs: HashMap<usize, Option<Rc<Vec<String>>>> = HashMap::new();
    let mut latest_llm: Option<Option<Rc<Vec<String>>>> = None;
    for (position, (span, (step_facts, llm))) in spans.iter().zip(extracted).enumerate() {
        if !step_facts.kind.is_step() {
            continue;
        }
        let agent = step_facts
            .agent
            .clone()
            .or_else(|| enclosing_agent(position));
        let catalog = match step_facts.kind {
            StepKind::Llm => {
                let catalog = catalog_of(&span.attributes).map(Rc::new);
                llm_catalogs.insert(position, catalog.clone());
                latest_llm = Some(catalog.clone());
                catalog
            }
            StepKind::Tool => {
                // The requesting model call: the tool span's parent when that
                // is a model call, else the latest model call before it.
                let parent = span
                    .parent_span_id
                    .as_deref()
                    .and_then(|parent| by_id.get(&(span.trace_id.as_str(), parent)))
                    .and_then(|position| llm_catalogs.get(position));
                match parent {
                    Some(catalog) => catalog.clone(),
                    None => latest_llm.clone().flatten(),
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
            files: OnceCell::new(),
            calls: OnceCell::new(),
            edit: OnceCell::new(),
            test: OnceCell::new(),
        });
    }
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

    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Str(text) => Value::String(text.to_string()),
            Self::Num(bits) => serde_json::Number::from_f64(f64::from_bits(*bits))
                .map_or(Value::Null, Value::Number),
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
}

/// The values a field offers a binding: one per element of a list, one for
/// a scalar, none when missing.
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
            let mut out: Vec<Captured> = Vec::new();
            for item in items.iter() {
                let captured = Captured::Str(Rc::from(item.as_str()));
                if !out.contains(&captured) {
                    out.push(captured);
                }
            }
            out
        }
    }
}

// -------------------------------------------------------------- evaluation

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

fn regex_matches(value: &Val<'_>, regex: &Regex) -> bool {
    match value {
        Val::Missing => false,
        Val::Text(text) => regex.is_match(text),
        Val::Num(number) => regex.is_match(&number.to_string()),
        Val::Bool(flag) => regex.is_match(if *flag { "true" } else { "false" }),
        Val::List(items) => items.iter().any(|item| regex.is_match(item)),
    }
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
fn compare(value: &Val<'_>, op: Op, literal: &Literal, env: &Env) -> bool {
    let target = resolve(literal, env);
    match op {
        Op::Eq => target.is_some_and(|target| equals(value, &target)),
        Op::Ne => !target.is_some_and(|target| equals(value, &target)),
        Op::Match | Op::NotMatch => {
            let hit = match &target {
                Some(Target::Regex(regex)) => regex_matches(value, regex),
                _ => false,
            };
            hit == (op == Op::Match)
        }
        Op::Gt | Op::Ge | Op::Lt | Op::Le => match target {
            Some(Target::Num(number)) => ordered(value, op, number),
            _ => false,
        },
    }
}

/// A predicate as a plain test: nothing binds (a binding position whose
/// capture is unbound is false).
pub(crate) fn test_pred(pred: &Pred, step: &Step<'_>, env: &Env) -> bool {
    match pred {
        Pred::And(items) => items.iter().all(|item| test_pred(item, step, env)),
        Pred::Or(items) => items.iter().any(|item| test_pred(item, step, env)),
        Pred::Not(inner) => !test_pred(inner, step, env),
        Pred::Cond(cond) => {
            let value = step.field(&cond.field);
            match &cond.test {
                Test::Truthy => truthy(&value),
                Test::Compare(op, literal) => compare(&value, *op, literal, env),
                Test::Bind(slot) => env.get(*slot).is_some_and(|captured| {
                    bindable(&value)
                        .iter()
                        .any(|candidate| candidate == captured)
                }),
            }
        }
    }
}

/// A predicate evaluated with bindings: the environments it holds under, in
/// priority order (empty when it fails). Binding positions fork one
/// environment per value of a multi-valued field.
fn eval_pred(pred: &Pred, step: &Step<'_>, env: Env, out: &mut Vec<Env>) {
    match pred {
        Pred::And(items) => {
            let mut current = vec![env];
            for item in items {
                let mut next = Vec::new();
                for env in current {
                    eval_pred(item, step, env, &mut next);
                }
                if next.is_empty() {
                    return;
                }
                current = next;
            }
            out.extend(current);
        }
        Pred::Cond(Cond {
            field,
            test: Test::Bind(slot),
        }) => {
            let value = step.field(field);
            let candidates = bindable(&value);
            match env.get(*slot) {
                Some(bound) => {
                    if candidates.iter().any(|candidate| candidate == bound) {
                        out.push(env);
                    }
                }
                None => {
                    for candidate in candidates {
                        out.push(env.with(*slot, candidate));
                    }
                }
            }
        }
        other => {
            if test_pred(other, step, &env) {
                out.push(env);
            }
        }
    }
}

/// Whether a step is of the atom's kind.
fn kind_matches(kind: AtomKind, step: &Step<'_>) -> bool {
    match kind {
        AtomKind::Step => true,
        AtomKind::Tool => step.facts.kind == StepKind::Tool,
        AtomKind::Llm => step.facts.kind == StepKind::Llm,
        AtomKind::Agent => step.facts.kind == StepKind::Agent,
        AtomKind::Edit => step.is_edit(),
        AtomKind::Test => step.is_test(),
    }
}

/// The values an atom's names are tested against: the files of an `edit`
/// atom, the step's name otherwise.
fn name_values<'s>(kind: AtomKind, step: &'s Step<'_>) -> Vec<&'s str> {
    match kind {
        AtomKind::Edit => step.files().iter().map(String::as_str).collect(),
        _ => vec![step.name()],
    }
}

/// An atom evaluated against one step: the environments it holds under, in
/// priority order. Empty when the step does not satisfy it.
pub(crate) fn eval_atom(atom: &Atom, step: &Step<'_>, env: &Env, out: &mut Vec<Env>) {
    if !kind_matches(atom.kind, step) {
        return;
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
                        return;
                    }
                }
                Some(Captured::Num(_)) => return,
                None => {
                    let mut forks: Vec<Env> = Vec::new();
                    for value in values {
                        let fork = env.with(*slot, Captured::Str(Rc::from(value)));
                        if !forks.contains(&fork) {
                            forks.push(fork);
                        }
                    }
                    forks
                }
            },
            names => {
                let hit = names.iter().any(|name| {
                    values.iter().any(|value| match name {
                        NameTest::Exact(expected) => value.eq_ignore_ascii_case(expected),
                        NameTest::Regex(regex) => regex.is_match(value),
                        NameTest::Capture { slot, .. } => {
                            matches!(env.get(*slot), Some(Captured::Str(bound)) if bound.as_ref() == *value)
                        }
                    })
                });
                if !hit {
                    return;
                }
                current
            }
        };
    }
    for pred in &atom.preds {
        let mut next = Vec::new();
        for env in current {
            eval_pred(pred, step, env, &mut next);
        }
        if next.is_empty() {
            return;
        }
        current = next;
    }
    out.extend(current);
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
        let field = |index: usize, field: Field| format!("{:?}", steps[index].field(&field));
        assert!(
            field(0, Field::Calls).contains("Bash") && field(0, Field::Calls).contains("Deploy")
        );
        assert!(field(0, Field::UnknownCalls).contains("Deploy"));
        assert!(!field(0, Field::UnknownCalls).contains("Bash"));
        assert_eq!(field(0, Field::Context), "Num(1200.0)");
        assert!(steps[1].is_test(), "pytest after && is a test run");
        assert!(!steps[0].is_test(), "an llm step is never a test run");
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
        assert_eq!(
            catalog_of(attributes.as_object().expect("object")),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        let text = json!({"gen_ai.tool.definitions": "[{\"name\": \"x\"}]"});
        assert_eq!(
            catalog_of(text.as_object().expect("object")),
            Some(vec!["x".into()])
        );
        assert_eq!(catalog_of(json!({}).as_object().expect("object")), None);
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
