//! Agent Blame: which agent step wrote this line?
//!
//! `git blame` says which commit last touched a line; when the code was
//! written by an agent, the commit is the least informative fact about it.
//! Traza holds every tool call the agent made, so it can answer the question
//! a reviewer actually has: **which session, which step, under which prompt,
//! with which model, introduced this exact line** — and open the run around
//! it.
//!
//! # What counts as introducing a line
//!
//! A tool step introduces a line into a file when its edit adds that line
//! more times than it removes it, compared on whitespace-normalised text
//! (runs of whitespace fold to one space, ends trimmed — so re-indenting a
//! line is not authorship, as with `git blame -w`). Edits are read from the
//! tool call's arguments, by shape:
//!
//! - `new_string` (with `old_string`) and a file path — Claude Code `Edit`;
//! - `edits: [{old_string, new_string}]` — `MultiEdit`;
//! - `content` with a file path, from a tool whose name contains `write` —
//!   `Write` (every line of the new content counts);
//! - `new_source` with `notebook_path` — `NotebookEdit`;
//! - `command` `create` / `str_replace` / `insert` with `path` — Anthropic's
//!   text-editor tool;
//! - `changes: {path: {type: add, content} | {type: update, unified_diff}}`
//!   — Codex file-change items (unified-diff `+`/`-` lines);
//! - any string in the arguments holding a Codex patch (`*** Begin Patch`):
//!   `+` lines count as added and `-` lines as removed, per file, under
//!   `*** Add File:` / `*** Update File:` (and `*** Move to:`). Relative patch
//!   paths are joined with the span's `code.cwd` (which the Codex importer
//!   records) or the arguments' `workdir`.
//!
//! The file matches when the query path is a suffix of the recorded path at a
//! path-component boundary (`src/lib.rs` matches `/repo/src/lib.rs`, not
//! `/repo/src/glib.rs`; a leading `/` is not an anchor), or — for a recorded
//! path that is still relative — the other way round. The CLI sends the path
//! relative to the repository root, so a line written in another checkout of
//! the same repository (a worktree) is still found.
//!
//! # How it is found, and what bounds it
//!
//! Candidates are the tool steps of the editing tools in [`EDIT_TOOLS`],
//! found through the exact attribute index on `gen_ai.tool.name` and
//! `tool.name`, so the search decodes edit calls and nothing else. The
//! content index cannot serve here: a store of agent transcripts is mostly
//! model calls carrying whole conversations, whose word filters admit nearly
//! every block for ordinary code words, and a content-driven search ends up
//! tokenising the store (measured on a real import: past a 30 s budget). Every
//! candidate is verified by parsing its edit. The whole search — one indexed
//! fold per tool name and key, then reading each match's run — spends one
//! query deadline; at most [`MAX_BLAME_CANDIDATES`] candidates are verified
//! (`truncated` says when more existed) and the earliest [`MAX_BLAME_LIMIT`]
//! matches are kept.
//!
//! **Limits, honestly:** a line written by a shell command (`sed -i`, a
//! heredoc, a code generator) is not an edit Traza can read and is not found,
//! nor is an edit by a tool whose name is not in [`EDIT_TOOLS`]; an argument
//! offloaded to the payload store is read only as its inline preview; a line
//! that was introduced, deleted and re-introduced
//! returns every introduction, earliest first — the earliest is the answer
//! the route is named for, not necessarily the one in the file today.

use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::semconv::{self, StepKind};
use crate::{Error, Result, Span, SpanFilter, Store};

/// The editing tools blame searches, by exact name: Claude Code's, Codex's
/// (`apply_patch` for its patch tool and file-change items; `shell` and
/// `local_shell` for older builds that applied patches through the shell),
/// Anthropic's text-editor tool under its published names, and common generic
/// names. A tool step is a candidate when `gen_ai.tool.name` or `tool.name`
/// is one of these.
pub const EDIT_TOOLS: &[&str] = &[
    "Edit",
    "MultiEdit",
    "Write",
    "NotebookEdit",
    "apply_patch",
    "shell",
    "local_shell",
    "str_replace_based_edit_tool",
    "str_replace_editor",
    "text_editor",
    "edit_file",
    "write_file",
    "create_file",
];

/// The attribute keys a tool name is indexed under.
const TOOL_NAME_KEYS: [&str; 2] = ["gen_ai.tool.name", "tool.name"];

/// Matches returned when the caller names no limit.
pub const DEFAULT_BLAME_LIMIT: usize = 5;

/// Matches returned at most.
pub const MAX_BLAME_LIMIT: usize = 50;

/// Candidate spans verified at most. Past it the answer is `truncated`.
pub const MAX_BLAME_CANDIDATES: usize = 200_000;

/// Characters of the prompt returned with a match.
pub const PROMPT_CHARS: usize = 500;

/// Parent links followed from a tool step to its prompt.
const MAX_ANCESTRY: usize = 64;

/// Spans of a run read to place a match on its tape.
const MAX_RUN_SPANS: usize = crate::tape::MAX_TAPE_SPANS;

/// One edit a tool step made to one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    /// The tool's name.
    pub tool: String,
    /// The file, as recorded (joined with the working directory when it was
    /// relative and one is known).
    pub file: String,
    /// Lines the edit wrote.
    pub added: Vec<String>,
    /// Lines the edit replaced.
    pub removed: Vec<String>,
}

impl Edit {
    /// Whether this edit introduced `normalized` (a [`normalize_line`]
    /// result): added more often than removed.
    pub fn introduces(&self, normalized: &str) -> bool {
        let count = |lines: &[String]| {
            lines
                .iter()
                .filter(|line| normalize_line(line) == normalized)
                .count()
        };
        count(&self.added) > count(&self.removed)
    }
}

/// One step that introduced the line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlameMatch {
    /// The run (session id, or `trace:<id>`).
    pub run: String,
    /// The run's tenant (`""` is the default tenant).
    pub tenant: String,
    /// The tool step's trace.
    pub trace_id: String,
    /// The tool step's span.
    pub span_id: String,
    /// The tool that made the edit.
    pub tool: String,
    /// The file as the edit recorded it.
    pub file: String,
    /// When the tool step started.
    pub at_ns: u64,
    /// The prompt that started the turn (or the task a sub-agent was given),
    /// cut to [`PROMPT_CHARS`] characters. Producer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The model whose response requested the edit.
    pub model: Option<String>,
    /// The step's position on its run's tape, from zero (`None` when the run
    /// is longer than a tape reads).
    pub step_index: Option<usize>,
    /// The sub-agent that made the edit, when it was not the main agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The emitting service (`claude-code`, `codex`, …).
    pub service: String,
}

/// A blame answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Blame {
    /// The steps that introduced the line, earliest first.
    pub matches: Vec<BlameMatch>,
    /// Candidate spans the content index admitted and the search verified.
    pub searched_spans: usize,
    /// Whether more than [`MAX_BLAME_CANDIDATES`] candidates existed, so an
    /// earlier introduction may have been missed.
    pub truncated: bool,
    /// The line as compared (whitespace-normalised).
    pub line: String,
    /// The path as compared.
    pub path: String,
}

/// Runs and traces read while enriching one answer, by `(tenant, id)`.
#[derive(Default)]
struct Reads {
    runs: std::collections::HashMap<(String, String), crate::run::RunSpans>,
    traces: std::collections::HashMap<(String, String), Vec<Span>>,
}

/// A candidate kept while folding: the span and the edit that matched.
struct Found {
    span: Span,
    tool: String,
    file: String,
}

impl Store {
    /// The earliest tool steps (at most `limit`, itself at most
    /// [`MAX_BLAME_LIMIT`]) whose edit introduced `line` into a file matching
    /// `path`, under `tenant` (`None` is the operator view across tenants).
    ///
    /// An empty line or path is [`Error::Invalid`]. Budgeted by the query
    /// deadline, anchored here, across the search and every match's
    /// enrichment.
    pub fn blame(
        &self,
        tenant: Option<&str>,
        path: &str,
        line: &str,
        limit: usize,
    ) -> Result<Blame> {
        let normalized = normalize_line(line);
        if normalized.is_empty() {
            return Err(Error::Invalid(
                "line is empty; pass the exact text of the line to blame".to_owned(),
            ));
        }
        let query_path = normalize_path(path);
        if query_path.trim_start_matches('/').is_empty() {
            return Err(Error::Invalid(
                "path is empty; pass the file's path or a suffix of it".to_owned(),
            ));
        }
        let limit = limit.clamp(1, MAX_BLAME_LIMIT);
        let deadline = self.query_deadline(Instant::now());
        let mut kept: Vec<Found> = Vec::new();
        let mut searched = 0usize;
        let mut truncated = false;
        for key in TOOL_NAME_KEYS {
            for tool in EDIT_TOOLS {
                let filter = SpanFilter {
                    attributes: vec![(key.to_owned(), Value::String((*tool).to_owned()))],
                    tenant: tenant.map(str::to_owned),
                    ..SpanFilter::default()
                };
                self.fold_spans_under(&filter, deadline, |span| {
                    if searched >= MAX_BLAME_CANDIDATES {
                        truncated = true;
                        return;
                    }
                    searched += 1;
                    consider(span, &normalized, &query_path, limit, &mut kept);
                })?;
            }
        }
        let mut matches = Vec::with_capacity(kept.len());
        let mut reads = Reads::default();
        for found in kept {
            matches.push(self.blame_match(found, deadline, &mut reads)?);
        }
        Ok(Blame {
            matches,
            searched_spans: searched,
            truncated,
            line: normalized,
            path: query_path,
        })
    }

    /// Reads a match's run (and its trace, when the run spans several) to
    /// name its prompt, model, agent and tape position. Matches in one
    /// session share one read of it (`reads`).
    fn blame_match(
        &self,
        found: Found,
        deadline: Option<crate::Deadline>,
        reads: &mut Reads,
    ) -> Result<BlameMatch> {
        let span = &found.span;
        let run = semconv::run_id(&span.attributes, &span.trace_id);
        let tenant = Some(span.tenant.as_str());
        let run_key = (span.tenant.clone(), run.clone());
        if !reads.runs.contains_key(&run_key) {
            let read = self.run_spans(tenant, &run, MAX_RUN_SPANS, deadline)?;
            reads.runs.insert(run_key.clone(), read);
        }
        let Reads { runs, traces } = reads;
        let run_spans = &runs[&run_key];
        let mut step_index = None;
        let mut steps = 0usize;
        for candidate in &run_spans.spans {
            let kind = semconv::step_kind_with(candidate, &semconv::facts(&candidate.attributes));
            if !kind.is_step() {
                continue;
            }
            if candidate.trace_id == span.trace_id && candidate.span_id == span.span_id {
                step_index = Some(steps);
                break;
            }
            steps += 1;
        }
        let one_trace = !run_spans.truncated
            && run_spans
                .spans
                .iter()
                .all(|candidate| candidate.trace_id == span.trace_id);
        let trace: &[Span] = if one_trace {
            &run_spans.spans
        } else {
            let trace_key = (span.tenant.clone(), span.trace_id.clone());
            if !traces.contains_key(&trace_key) {
                let read = self.trace_spans_in(tenant, &span.trace_id, deadline)?;
                traces.insert(trace_key.clone(), read);
            }
            &traces[&trace_key]
        };
        let by_id: std::collections::HashMap<&str, &Span> = trace
            .iter()
            .map(|candidate| (candidate.span_id.as_str(), candidate))
            .collect();
        let (mut model, mut prompt, mut agent) = (None, None, None);
        let mut parent = span.parent_span_id.as_deref();
        for _ in 0..MAX_ANCESTRY {
            let Some(ancestor) = parent.and_then(|id| by_id.get(id)) else {
                break;
            };
            let facts = semconv::facts(&ancestor.attributes);
            let kind = semconv::step_kind_with(ancestor, &facts);
            if model.is_none() && kind == StepKind::Llm {
                model = facts.model.clone();
            }
            if agent.is_none() && prompt.is_none() && kind == StepKind::Agent {
                agent = semconv::step_facts(ancestor).agent;
            }
            if prompt.is_none() {
                prompt = ancestor
                    .events
                    .iter()
                    .rev()
                    .find(|event| event.name == "gen_ai.user.message")
                    .and_then(|event| event.attributes.get("content"))
                    .and_then(Value::as_str)
                    .map(|text| crate::import::preview(text, PROMPT_CHARS));
            }
            if model.is_some() && prompt.is_some() {
                break;
            }
            parent = ancestor.parent_span_id.as_deref();
        }
        Ok(BlameMatch {
            run,
            tenant: span.tenant.clone(),
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
            tool: found.tool,
            file: found.file,
            at_ns: span.start_time_ns,
            prompt,
            model,
            step_index,
            agent,
            service: span.service.clone(),
        })
    }
}

/// Keeps `span` among the earliest `limit` candidates when one of its edits
/// introduced `normalized` into a file matching `query_path`. A span seen
/// twice (indexed under both tool-name keys) is kept once.
fn consider(span: &Span, normalized: &str, query_path: &str, limit: usize, kept: &mut Vec<Found>) {
    let Some(edit) = edits(span)
        .into_iter()
        .find(|edit| edit.introduces(normalized) && path_matches(&edit.file, query_path))
    else {
        return;
    };
    let key = |found: &Found| {
        (
            found.span.start_time_ns,
            found.span.tenant.clone(),
            found.span.trace_id.clone(),
            found.span.span_id.clone(),
        )
    };
    let candidate = Found {
        span: span.clone(),
        tool: edit.tool,
        file: edit.file,
    };
    match kept.binary_search_by(|probe| key(probe).cmp(&key(&candidate))) {
        Ok(_) => {}
        Err(at) if at < limit => {
            kept.insert(at, candidate);
            kept.truncate(limit);
        }
        Err(_) => {}
    }
}

/// Whitespace-normalised line text: runs of whitespace fold to one space,
/// ends trimmed.
pub fn normalize_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A path with `\` as `/`, repeated and `.` components removed, and no
/// trailing slash.
pub fn normalize_path(path: &str) -> String {
    let unified = path.trim().replace('\\', "/");
    let absolute = unified.starts_with('/');
    let parts: Vec<&str> = unified
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Whether recorded path `recorded` is the file `query` names: `query` is a
/// suffix of it at a component boundary, or — when `recorded` is relative —
/// it is a suffix of `query`.
pub fn path_matches(recorded: &str, query: &str) -> bool {
    let recorded = normalize_path(recorded);
    let query = normalize_path(query);
    if recorded.is_empty() || query.is_empty() {
        return false;
    }
    let suffix = |whole: &str, tail: &str| {
        let tail = tail.trim_start_matches('/');
        let whole_trimmed = whole.trim_start_matches('/');
        whole_trimmed == tail
            || (whole_trimmed.ends_with(tail)
                && whole_trimmed.as_bytes()[whole_trimmed.len() - tail.len() - 1] == b'/')
    };
    suffix(&recorded, &query) || (!recorded.starts_with('/') && suffix(&query, &recorded))
}

/// The edits a tool step's arguments describe (empty for anything that is
/// not a tool step, or not an edit).
pub fn edits(span: &Span) -> Vec<Edit> {
    let facts = semconv::facts(&span.attributes);
    if semconv::step_kind_with(span, &facts) != StepKind::Tool {
        return Vec::new();
    }
    let tool = ["gen_ai.tool.name", "tool.name"]
        .iter()
        .find_map(|key| span.attributes.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let Some(arguments) = [
        "gen_ai.tool.call.arguments",
        "traceloop.entity.input",
        "input.value",
    ]
    .iter()
    .find_map(|key| span.attributes.get(*key)) else {
        return Vec::new();
    };
    let arguments = match arguments {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => Value::String(text.clone()),
        },
        other => other.clone(),
    };
    let cwd = span
        .attributes
        .get("code.cwd")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("workdir").and_then(Value::as_str))
        .map(str::to_owned);
    let mut out = Vec::new();
    if let Value::Object(map) = &arguments {
        let file = ["file_path", "path", "filePath", "notebook_path"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(|file| join(cwd.as_deref(), file));
        let text = |key: &str| map.get(key).and_then(Value::as_str);
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        if let Some(file) = &file {
            let lower = tool.to_ascii_lowercase();
            if let Some(new) = text("new_string") {
                out.push(Edit {
                    tool: tool.clone(),
                    file: file.clone(),
                    added: lines(new),
                    removed: text("old_string").map(lines).unwrap_or_default(),
                });
            } else if let Some(Value::Array(items)) = map.get("edits") {
                let mut edit = Edit {
                    tool: tool.clone(),
                    file: file.clone(),
                    ..Edit::default()
                };
                for item in items {
                    if let Some(new) = item.get("new_string").and_then(Value::as_str) {
                        edit.added.extend(lines(new));
                    }
                    if let Some(old) = item.get("old_string").and_then(Value::as_str) {
                        edit.removed.extend(lines(old));
                    }
                }
                out.push(edit);
            } else if let Some(source) = text("new_source") {
                if text("edit_mode") != Some("delete") {
                    out.push(Edit {
                        tool: tool.clone(),
                        file: file.clone(),
                        added: lines(source),
                        removed: Vec::new(),
                    });
                }
            } else if let Some(command) = text("command") {
                let (added, removed) = match command {
                    "create" => (text("file_text"), None),
                    "str_replace" => (text("new_str"), text("old_str")),
                    "insert" => (text("new_str"), None),
                    _ => (None, None),
                };
                if let Some(added) = added {
                    out.push(Edit {
                        tool: tool.clone(),
                        file: file.clone(),
                        added: lines(added),
                        removed: removed.map(lines).unwrap_or_default(),
                    });
                }
            } else if lower.contains("write") {
                if let Some(content) = text("content") {
                    out.push(Edit {
                        tool: tool.clone(),
                        file: file.clone(),
                        added: lines(content),
                        removed: Vec::new(),
                    });
                }
            }
        }
        // Codex file-change items: `{"changes": {path: {"type": "add",
        // "content"} | {"type": "update", "unified_diff", "move_path"}}}`.
        if let Some(Value::Object(changes)) = map.get("changes") {
            for (path, change) in changes {
                let target = change
                    .get("move_path")
                    .and_then(Value::as_str)
                    .unwrap_or(path);
                let mut edit = Edit {
                    tool: tool.clone(),
                    file: join(cwd.as_deref(), target),
                    ..Edit::default()
                };
                match change.get("type").and_then(Value::as_str) {
                    Some("add") => {
                        edit.added = change
                            .get("content")
                            .and_then(Value::as_str)
                            .map(lines)
                            .unwrap_or_default();
                    }
                    Some("update") => {
                        let diff = change
                            .get("unified_diff")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let (added, removed) = parse_unified_diff(diff);
                        edit.added = added;
                        edit.removed = removed;
                    }
                    _ => continue,
                }
                out.push(edit);
            }
        }
    }
    if out.is_empty() {
        let mut patches = Vec::new();
        collect_patches(&arguments, &mut patches, 0);
        for patch in patches {
            for mut edit in parse_patch(&patch) {
                edit.tool.clone_from(&tool);
                edit.file = join(cwd.as_deref(), &edit.file);
                out.push(edit);
            }
        }
    }
    out
}

/// Every string in `value` that holds a Codex patch.
fn collect_patches(value: &Value, out: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    match value {
        Value::String(text) if text.contains("*** Begin Patch") => out.push(text.clone()),
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_patches(item, out, depth + 1)),
        Value::Object(map) => map
            .values()
            .for_each(|item| collect_patches(item, out, depth + 1)),
        _ => {}
    }
}

/// Parses a Codex patch (`*** Begin Patch` … `*** End Patch`) into one edit
/// per file it adds or updates.
pub fn parse_patch(text: &str) -> Vec<Edit> {
    let mut out: Vec<Edit> = Vec::new();
    let Some(start) = text.find("*** Begin Patch") else {
        return out;
    };
    let mut current: Option<usize> = None;
    for line in text[start..].lines().skip(1) {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            out.push(Edit {
                file: path.trim().to_owned(),
                ..Edit::default()
            });
            current = Some(out.len() - 1);
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            out.push(Edit {
                file: path.trim().to_owned(),
                ..Edit::default()
            });
            current = Some(out.len() - 1);
        } else if line.starts_with("*** Delete File: ") {
            current = None;
        } else if let Some(path) = line.strip_prefix("*** Move to: ") {
            if let Some(at) = current {
                out[at].file = path.trim().to_owned();
            }
        } else if line.starts_with("*** End Patch") {
            break;
        } else if line.starts_with("***") || line.starts_with("@@") {
            continue;
        } else if let Some(at) = current {
            if let Some(added) = line.strip_prefix('+') {
                out[at].added.push(added.to_owned());
            } else if let Some(removed) = line.strip_prefix('-') {
                out[at].removed.push(removed.to_owned());
            }
        }
    }
    out
}

/// The added and removed lines of a unified diff's hunks (`+`/`-` lines,
/// not the `+++`/`---` file headers).
pub fn parse_unified_diff(diff: &str) -> (Vec<String>, Vec<String>) {
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            added.push(text.to_owned());
        } else if let Some(text) = line.strip_prefix('-') {
            removed.push(text.to_owned());
        }
    }
    (added, removed)
}

/// `file` joined onto `cwd` when it is relative and a directory is known.
fn join(cwd: Option<&str>, file: &str) -> String {
    let unified = file.replace('\\', "/");
    match cwd {
        Some(cwd) if !unified.starts_with('/') && !cwd.is_empty() => {
            normalize_path(&format!("{}/{unified}", cwd.trim_end_matches('/')))
        }
        _ => normalize_path(&unified),
    }
}

#[cfg(test)]
mod tests;
