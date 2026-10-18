//! TQL — trajectory queries: regular expressions over a run's steps.
//!
//! A run is a sequence of steps (model calls, tool calls, agent
//! invocations — [`crate::tape`]). TQL asks whether that sequence contains
//! a shape: *a failing test, then an edit to a test file, then a passing
//! test*; *the same file edited five times within ten steps*; *a tool the
//! model was never offered*. Atoms match one step each (`tool(bash)`,
//! `llm(gpt-4o)`, `agent(planner)`, `edit`, `test`, `step`) and carry
//! predicates (`[status=error]`, `[args~"pytest"]`, `[cost>0.5]`,
//! `[duration>30s]`); `->` joins adjacent steps and `..>` joins a step to a
//! later one; `*`, `+`, `?`, `{n,m}`, `|` and `( )` work as in a regex;
//! `@name` captures a value and requires it equal wherever it reappears;
//! `within N` / `within 5m` bounds a whole match.
//!
//! The pieces:
//!
//! - [`parse`](self) — lexer and recursive-descent parser; every refusal
//!   names its 1-based character column ([`TqlError`]).
//! - `regex` — the std-only, linear-time regex subset behind `~`.
//! - `nfa` — compilation to an NFA and a Pike-VM matcher with
//!   leftmost-first semantics, property-tested against a backtracking
//!   reference.
//! - `step` — what a predicate reads from a step.
//! - [`detectors`] — the named behaviour library.
//!
//! On the store, [`Store::trajectory_search`] runs one query over the most
//! recent runs of a window and [`Store::detector_findings`] runs the whole
//! library. Both are bounded at every stage — candidate runs, spans per
//! run, text per field, regex instructions per query, environments per
//! evaluation, paths and remembered states per step, matches kept — and
//! spend ONE compute budget ([`crate::Config::query_deadline`]):
//! enumerating the window's runs refuses when the budget is gone, while
//! scanning them (most recent first) stops where the budget ran out and
//! says so (`stopped: "deadline"`), so a partial answer is never mistaken
//! for a complete one. Inside a run the matcher charges its work — paths
//! advanced, environments forked, regex instructions stepped over text —
//! to a meter that consults the deadline in proportion (`meter`), so no
//! predicate or capture can run past the budget.
//!
//! Everything is deterministic computation over stored spans: no model is
//! called, nothing leaves the process. The guide is `docs/guide/tql.md`.

mod ast;
pub mod detectors;
mod meter;
mod nfa;
mod parse;
mod regex;
mod step;

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::attribution::{self, Outcome};
use crate::semconv::{self, StepKind};
use crate::{Deadline, Error, Result, SpanFilter, Store};

pub use detectors::{Detector, Severity, DETECTORS};

/// Window searched when the caller names no start: the last 30 days.
pub const DEFAULT_WINDOW_NS: u64 = 30 * 86_400 * 1_000_000_000;

/// Runs returned by a search when the caller names no limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 50;

/// Runs returned by a search at most.
pub const MAX_SEARCH_LIMIT: usize = 500;

/// Most recent runs of a window a search considers.
pub const MAX_SEARCH_RUNS: usize = 5_000;

/// Most recent runs of a window the detector findings consider.
pub const MAX_FINDINGS_RUNS: usize = 2_000;

/// Spans read per run; a longer run is matched on its earliest spans and
/// says so (`run_truncated`), as a tape does.
pub const MAX_RUN_SPANS: usize = crate::tape::MAX_TAPE_SPANS;

/// Occurrences counted per run.
pub const MAX_OCCURRENCES_COUNTED: usize = 1_000;

/// Occurrences listed per run when one run is searched (`run=`).
pub const MAX_OCCURRENCES_LISTED: usize = 100;

/// Steps listed per occurrence.
pub const MAX_MATCH_SPANS: usize = 100;

/// Example runs listed per detector.
pub const MAX_DETECTOR_EXAMPLES: usize = 5;

/// Bytes of one captured value a response carries. Matching uses the whole
/// value (up to the 64 KiB text cap); only the report is cut.
pub const MAX_CAPTURE_BYTES: usize = 1024;

/// A query refused by the parser, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TqlError {
    /// What is wrong, and usually how to fix it.
    pub message: String,
    /// The 1-based character column it was found at.
    pub column: usize,
}

impl fmt::Display for TqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "column {}: {}", self.column, self.message)
    }
}

impl std::error::Error for TqlError {}

impl TqlError {
    /// The query with a caret under the column, for a terminal or an agent.
    pub fn pointed(&self, query: &str) -> String {
        let line: String = query
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        format!(
            "{line}\n{}^\n{}",
            " ".repeat(self.column.saturating_sub(1)),
            self
        )
    }
}

/// How far a whole match may stretch (`within`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Within {
    /// The match covers at most this many steps, gap steps included.
    Steps(u64),
    /// The match's last step starts at most this many nanoseconds after its
    /// first step starts.
    Duration(u64),
}

impl Within {
    fn to_json(self) -> Value {
        match self {
            Self::Steps(steps) => json!({"steps": steps}),
            Self::Duration(nanos) => json!({"duration_ns": nanos}),
        }
    }
}

/// A compiled TQL query.
pub struct Query {
    source: String,
    pub(crate) node: ast::Node,
    pub(crate) program: nfa::Program,
    captures: Vec<String>,
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("source", &self.source)
            .finish()
    }
}

/// What a query was read as, for `GET /v1/trajectories/explain`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Explain {
    /// The canonical rendering: grouping made explicit wherever precedence
    /// decided it.
    pub query: String,
    /// Capture names, in first-use order.
    pub captures: Vec<String>,
    /// Fields the predicates read.
    pub fields: Vec<String>,
    /// Atoms written.
    pub atoms: usize,
    /// The fewest steps a match covers.
    pub min_steps: u64,
    /// The most, or `None` when unbounded.
    pub max_steps: Option<u64>,
    /// The `within` bound, as `{"steps": n}` or `{"duration_ns": n}`.
    pub within: Option<Value>,
    /// Instructions in the compiled program.
    pub instructions: usize,
}

impl Query {
    /// Parses and compiles `text`.
    pub fn compile(text: &str) -> std::result::Result<Self, TqlError> {
        let parsed = parse::parse(text)?;
        let program = nfa::compile(&parsed.node, parsed.captures.len(), parsed.within)
            .map_err(|message| TqlError { message, column: 1 })?;
        Ok(Self {
            source: text.to_owned(),
            node: parsed.node,
            program,
            captures: parsed.captures,
        })
    }

    /// The query as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The canonical rendering (parsing it yields the same query).
    pub fn normalized(&self) -> String {
        let mut out = ast::render(&self.node, &self.captures);
        match self.program.within {
            Some(Within::Steps(steps)) => out.push_str(&format!(" within {steps}")),
            Some(Within::Duration(nanos)) => {
                out.push_str(&format!(" within {}", ast::render_duration(nanos)))
            }
            None => {}
        }
        out
    }

    /// Capture names, in first-use order.
    pub fn captures(&self) -> &[String] {
        &self.captures
    }

    /// What the query was read as.
    pub fn explain(&self) -> Explain {
        let (min_steps, max_steps) = ast::step_bounds(&self.node);
        Explain {
            query: self.normalized(),
            captures: self.captures.clone(),
            fields: ast::fields_used(&self.node),
            atoms: ast::atom_count(&self.node),
            min_steps,
            max_steps,
            within: self.program.within.map(Within::to_json),
            instructions: self.program.insts.len(),
        }
    }
}

/// The fields a predicate can read, with their kind and meaning, for help
/// text and the dashboard's syntax help.
pub fn fields() -> Vec<Value> {
    let mut out: Vec<Value> = ast::FIELDS
        .iter()
        .map(|(name, ty, about)| json!({"name": name, "type": type_word(*ty), "about": about}))
        .collect();
    for (name, about) in [
        (
            "args.<path>",
            "a value inside the step's input, by key (and array index)",
        ),
        ("result.<path>", "a value inside the step's output"),
        (
            "attr.<key>",
            "any span attribute, verbatim (attr.http.status_code)",
        ),
    ] {
        out.push(json!({"name": name, "type": "dynamic", "about": about}));
    }
    out
}

fn type_word(ty: ast::FieldType) -> &'static str {
    match ty {
        ast::FieldType::Text => "text",
        ast::FieldType::Number => "number",
        ast::FieldType::Duration => "duration",
        ast::FieldType::Bool => "bool",
        ast::FieldType::List => "list",
        ast::FieldType::Dynamic => "dynamic",
    }
}

// ------------------------------------------------------------------ search

/// Which runs' matches a search keeps, by how they ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutcomeFilter {
    /// Every run.
    #[default]
    Any,
    /// Runs that succeeded.
    Success,
    /// Runs that failed.
    Failure,
    /// Runs that were abandoned.
    Abandoned,
    /// Runs whose outcome cannot be told (still active, or truncated).
    Unknown,
}

impl OutcomeFilter {
    /// Parses `any`, `success`, `failure`, `abandoned` or `unknown`.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "any" => Self::Any,
            "success" => Self::Success,
            "failure" => Self::Failure,
            "abandoned" => Self::Abandoned,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    fn admits(self, outcome: Outcome) -> bool {
        match self {
            Self::Any => true,
            Self::Success => outcome == Outcome::Success,
            Self::Failure => outcome == Outcome::Failure,
            Self::Abandoned => outcome == Outcome::Abandoned,
            Self::Unknown => outcome == Outcome::Unknown,
        }
    }
}

/// What a search reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    /// Window start (span start times), default [`DEFAULT_WINDOW_NS`] ago.
    pub since_ns: Option<u64>,
    /// Window end, default unbounded.
    pub until_ns: Option<u64>,
    /// Only runs of this agent: the first step's agent name, else the run's
    /// earliest span's service.
    pub agent: Option<String>,
    /// Only runs that ended this way.
    pub outcome: OutcomeFilter,
    /// Runs returned at most (clamped to [`MAX_SEARCH_LIMIT`]).
    pub limit: usize,
    /// Search this one run only, listing every occurrence (no window).
    pub run: Option<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            since_ns: None,
            until_ns: None,
            agent: None,
            outcome: OutcomeFilter::Any,
            limit: DEFAULT_SEARCH_LIMIT,
            run: None,
        }
    }
}

/// One step of a match.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MatchSpan {
    /// The step's trace.
    pub trace_id: String,
    /// The step's span.
    pub span_id: String,
    /// Its position among the run's steps (the tape's index).
    pub step_index: usize,
    /// llm, tool or agent.
    pub kind: StepKind,
    /// The tool, model or agent name (else the span name).
    pub name: String,
}

/// One occurrence of the pattern in a run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Occurrence {
    /// The first step the match covers.
    pub first_step: usize,
    /// The last step it covers, gap steps included.
    pub last_step: usize,
    /// The steps the pattern's atoms matched, in order (gap steps are not
    /// listed), at most [`MAX_MATCH_SPANS`].
    pub spans: Vec<MatchSpan>,
    /// Whether `spans` was cut to [`MAX_MATCH_SPANS`].
    pub spans_truncated: bool,
    /// Captured values by name, each text cut to [`MAX_CAPTURE_BYTES`].
    /// Producer data.
    pub captures: Map<String, Value>,
    /// Whether a captured value was cut.
    pub captures_truncated: bool,
}

/// One run that matched.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunMatch {
    /// The run id.
    pub run: String,
    /// The run's tenant (`""` is the default tenant).
    pub tenant: String,
    /// The run's agent: its first step's agent name, else its earliest
    /// span's service.
    pub agent: Option<String>,
    /// How the run ended (`unknown` while it is still active, or when it
    /// was longer than the span budget).
    pub outcome: Outcome,
    /// The first occurrence's steps.
    pub spans: Vec<MatchSpan>,
    /// Whether the first occurrence's steps were cut.
    pub spans_truncated: bool,
    /// The first occurrence's captures, each text cut to
    /// [`MAX_CAPTURE_BYTES`].
    pub captures: Map<String, Value>,
    /// Whether one of those captured values was cut.
    pub captures_truncated: bool,
    /// The first occurrence's first step.
    pub first_step: usize,
    /// The first occurrence's last step.
    pub last_step: usize,
    /// Non-overlapping occurrences in the run, at most
    /// [`MAX_OCCURRENCES_COUNTED`].
    pub count: usize,
    /// Whether counting stopped at the cap.
    pub count_capped: bool,
    /// Steps in the run (as read).
    pub steps: usize,
    /// The latest span start in the run.
    pub last_activity_ns: u64,
    /// Whether the run had more than [`MAX_RUN_SPANS`] spans, so only its
    /// beginning was matched.
    pub run_truncated: bool,
    /// Whether the matcher's path cap dropped paths in this run, so an
    /// occurrence may have been missed.
    pub inexact: bool,
    /// Every occurrence, when one run was searched (`run=`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<Occurrence>,
}

/// Why a scan stopped before the window's last candidate run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stopped {
    /// The result limit was reached.
    Limit,
    /// The compute budget ran out.
    Deadline,
}

/// A search answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchResult {
    /// The query as it was read (canonical rendering).
    pub query: String,
    /// Matching runs, most recent first.
    pub matches: Vec<RunMatch>,
    /// Candidate runs examined, newest first: read and matched, or left out
    /// by the `agent` filter, or gone or unreadable when reached.
    pub runs_scanned: usize,
    /// Candidate runs in the window (at most [`MAX_SEARCH_RUNS`]).
    pub runs_in_window: usize,
    /// Whether there may be matches this answer does not list: the scan
    /// stopped early, or the window held more runs than are considered.
    pub truncated: bool,
    /// Why the scan stopped early, if it did.
    pub stopped: Option<Stopped>,
    /// Whether the window held more runs than [`MAX_SEARCH_RUNS`] (only the
    /// most recent were considered).
    pub window_capped: bool,
    /// When runs went unchecked — the scan stopped early, or the window was
    /// capped — the latest activity of the newest run not checked (for a
    /// capped window, of the oldest run considered: every run left out was
    /// active no later). Repeating the search with this as `until`
    /// continues (a run active on both sides of the instant can appear in
    /// both answers).
    pub next_until_ns: Option<u64>,
    /// Spans in the window left out because their run id cannot be read
    /// back to them: a session id of the `trace:` form naming another trace
    /// (or none).
    pub spans_skipped: usize,
    /// Candidate runs left out because reading them by id was refused.
    pub runs_skipped: usize,
    /// The window searched (absent when one run was searched).
    pub since_ns: Option<u64>,
    /// The window's end, when bounded.
    pub until_ns: Option<u64>,
}

/// The bounds of a scan's two phases. In production both deadlines are the
/// same request deadline and nothing else is set; they are separate, and the
/// rest exists, so a test can spend one phase and not the other, stop a scan
/// partway, or cap a small window, without racing a clock.
#[derive(Clone, Copy)]
struct Budget {
    /// Enumerating the window's runs: all or nothing.
    enumerate: Option<Deadline>,
    /// Reading and matching them, newest first: stops where it runs out.
    scan: Option<Deadline>,
    /// Most recent runs of the window considered.
    window_cap: usize,
    /// Runs the scan may check before its budget counts as spent (`None`:
    /// only the deadline stops it).
    scan_runs: Option<usize>,
}

impl Budget {
    /// One request's budget: `deadline` for both phases.
    fn request(deadline: Option<Deadline>, window_cap: usize) -> Self {
        Self {
            enumerate: deadline,
            scan: deadline,
            window_cap,
            scan_runs: None,
        }
    }

    /// Errs as the deadline does once the scan has checked `scanned` runs
    /// and may check no more.
    fn check_run(&self, scanned: usize) -> Result<()> {
        if self.scan_runs.is_some_and(|runs| scanned >= runs) {
            return Err(Error::DeadlineExceeded(format!(
                "the scan's budget allowed {scanned} runs"
            )));
        }
        Deadline::check(self.scan, 0)
    }
}

/// A window's candidate run.
struct Candidate {
    tenant: String,
    run: String,
    latest: u64,
}

/// A window's candidate runs.
struct Window {
    /// The most recent runs, newest first.
    candidates: Vec<Candidate>,
    /// Whether the window held more runs than the cap.
    capped: bool,
    /// Spans left out because their run id does not read back to them.
    spans_skipped: usize,
}

/// What reading a candidate run found.
enum Read {
    Spans(crate::run::RunSpans),
    /// No spans: erased between enumeration and read.
    Gone,
    /// The run id was refused when read back. Enumeration leaves out every
    /// id it knows to be unreadable; this keeps one that slips through from
    /// failing the whole request.
    Unreadable,
}

/// One run read for matching.
struct ReadRun<'s> {
    steps: Vec<step::Step<'s>>,
    agent: Option<String>,
    truncated: bool,
}

/// Whether a span's run id reads back to the span. A run id of the
/// `trace:<id>` form is read as trace `<id>`, so a span whose SESSION id
/// takes that form while naming another trace (or none) would be listed
/// under a run that does not contain it — or one that cannot be read.
fn reads_back(run: &str, trace_id: &str) -> bool {
    semconv::run_trace_id(run).map_or(true, |trace| !trace.is_empty() && trace == trace_id)
}

impl Store {
    /// The `cap` runs with the most recent activity among spans starting
    /// in `[since, until]`, newest first, and whether the window held more.
    ///
    /// Exact under the cap: a run is kept or evicted by its latest start
    /// seen so far, and since that only grows, a run evicted (or never
    /// admitted) cannot belong to the final top `cap`. A span whose run id
    /// does not [read back](reads_back) to it is left out and counted.
    fn window_runs(
        &self,
        tenant: Option<&str>,
        since_ns: u64,
        until_ns: Option<u64>,
        cap: usize,
        deadline: Option<Deadline>,
    ) -> Result<Window> {
        let filter = SpanFilter {
            tenant: tenant.map(str::to_owned),
            since_ns: Some(since_ns),
            until_ns,
            ..SpanFilter::default()
        };
        let mut latest: HashMap<(String, String), u64> = HashMap::new();
        // The eviction order is needed only once the map is full: below the
        // cap, the map alone is the answer, and most windows stay below it.
        let mut order: Option<BTreeSet<(u64, String, String)>> = None;
        let mut capped = false;
        let mut spans_skipped = 0;
        self.fold_spans_under(&filter, deadline, |span| {
            let run = semconv::run_id(&span.attributes, &span.trace_id);
            if !reads_back(&run, &span.trace_id) {
                spans_skipped += 1;
                return;
            }
            let key = (span.tenant.clone(), run);
            let start = span.start_time_ns;
            if let Some(known) = latest.get_mut(&key) {
                if start > *known {
                    if let Some(order) = order.as_mut() {
                        order.remove(&(*known, key.0.clone(), key.1.clone()));
                        order.insert((start, key.0.clone(), key.1.clone()));
                    }
                    *known = start;
                }
                return;
            }
            if latest.len() >= cap {
                capped = true;
                let order = order.get_or_insert_with(|| {
                    latest
                        .iter()
                        .map(|((tenant, run), start)| (*start, tenant.clone(), run.clone()))
                        .collect()
                });
                let Some(oldest) = order.first().cloned() else {
                    return;
                };
                if start <= oldest.0 {
                    return;
                }
                order.remove(&oldest);
                latest.remove(&(oldest.1, oldest.2));
                order.insert((start, key.0.clone(), key.1.clone()));
            }
            latest.insert(key, start);
        })?;
        let mut candidates: Vec<Candidate> = latest
            .into_iter()
            .map(|((tenant, run), latest)| Candidate {
                tenant,
                run,
                latest,
            })
            .collect();
        candidates.sort_by(|left, right| {
            right
                .latest
                .cmp(&left.latest)
                .then_with(|| left.tenant.cmp(&right.tenant))
                .then_with(|| left.run.cmp(&right.run))
        });
        Ok(Window {
            candidates,
            capped,
            spans_skipped,
        })
    }

    /// Reads one run's spans and steps within its own tenant. `None` when
    /// the run has no spans (erased between enumeration and read).
    fn read_run_spans(
        &self,
        tenant: Option<&str>,
        run: &str,
        deadline: Option<Deadline>,
    ) -> Result<Option<crate::run::RunSpans>> {
        let spans = self.run_spans(tenant, run, MAX_RUN_SPANS, deadline)?;
        Ok((!spans.spans.is_empty()).then_some(spans))
    }

    /// Reads a window's candidate run; a refused id is [`Read::Unreadable`]
    /// rather than an error, so one bad id cannot fail the request.
    fn read_candidate(&self, candidate: &Candidate, deadline: Option<Deadline>) -> Result<Read> {
        match self.read_run_spans(Some(&candidate.tenant), &candidate.run, deadline) {
            Ok(Some(spans)) => Ok(Read::Spans(spans)),
            Ok(None) => Ok(Read::Gone),
            Err(Error::Invalid(_)) => Ok(Read::Unreadable),
            Err(other) => Err(other),
        }
    }

    fn read_steps<'s>(
        &self,
        spans: &'s crate::run::RunSpans,
        deadline: Option<Deadline>,
    ) -> Result<ReadRun<'s>> {
        let steps = step::build_steps(&spans.spans, |span| self.step_facts(span), deadline)?;
        let agent = steps
            .first()
            .and_then(|step| step.facts.agent.clone())
            .or_else(|| spans.spans.first().map(|span| span.service.clone()));
        Ok(ReadRun {
            steps,
            agent,
            truncated: spans.truncated,
        })
    }

    /// Runs `query` over the most recent runs of a window (or over one run,
    /// with [`SearchOptions::run`]) under `tenant` — the scoping of
    /// [`Store::get_run_in`]: `Some(t)` is tenant `t` alone, `None` the
    /// operator view, each run read within its own tenant.
    ///
    /// One compute budget covers the request. Enumerating the window's
    /// runs refuses with [`Error::DeadlineExceeded`] when it runs out;
    /// scanning them, most recent first, stops where it ran out and says so
    /// (`stopped: "deadline"`, `truncated: true`).
    pub fn trajectory_search(
        &self,
        tenant: Option<&str>,
        query: &Query,
        options: &SearchOptions,
        now_ns: u64,
    ) -> Result<SearchResult> {
        let deadline = self.query_deadline(Instant::now());
        self.trajectory_search_under(
            tenant,
            query,
            options,
            now_ns,
            Budget::request(deadline, MAX_SEARCH_RUNS),
        )
    }

    /// [`Self::trajectory_search`] under explicit bounds, so tests can reach
    /// a scan the budget interrupts without racing a clock.
    fn trajectory_search_under(
        &self,
        tenant: Option<&str>,
        query: &Query,
        options: &SearchOptions,
        now_ns: u64,
        budget: Budget,
    ) -> Result<SearchResult> {
        let deadline = budget.scan;
        let limit = options.limit.clamp(1, MAX_SEARCH_LIMIT);
        let mut result = SearchResult {
            query: query.normalized(),
            matches: Vec::new(),
            runs_scanned: 0,
            runs_in_window: 0,
            truncated: false,
            stopped: None,
            window_capped: false,
            next_until_ns: None,
            spans_skipped: 0,
            runs_skipped: 0,
            since_ns: None,
            until_ns: None,
        };
        // One run: every occurrence, no window.
        if let Some(run) = &options.run {
            if run.is_empty() {
                return Err(Error::Invalid("run= names no run".to_owned()));
            }
            let Some(spans) = self.read_run_spans(tenant, run, deadline)? else {
                return Ok(result);
            };
            let read = self.read_steps(&spans, deadline)?;
            result.runs_scanned = 1;
            result.runs_in_window = 1;
            let tenant_name = spans
                .spans
                .first()
                .map(|span| span.tenant.clone())
                .unwrap_or_default();
            if let Some(found) = self.match_run(
                query,
                &spans,
                &read,
                run,
                &tenant_name,
                options,
                MAX_OCCURRENCES_LISTED,
                OutcomeOf::At(now_ns),
                deadline,
            )? {
                result.matches.push(found);
            }
            return Ok(result);
        }
        let since = options
            .since_ns
            .unwrap_or_else(|| now_ns.saturating_sub(DEFAULT_WINDOW_NS));
        result.since_ns = Some(since);
        result.until_ns = options.until_ns;
        let window = self.window_runs(
            tenant,
            since,
            options.until_ns,
            budget.window_cap,
            budget.enumerate,
        )?;
        let candidates = &window.candidates;
        result.runs_in_window = candidates.len();
        result.window_capped = window.capped;
        result.spans_skipped = window.spans_skipped;
        for (index, candidate) in candidates.iter().enumerate() {
            if result.matches.len() >= limit {
                result.stopped = Some(Stopped::Limit);
                result.truncated = true;
                result.next_until_ns = Some(candidate.latest);
                break;
            }
            let outcome = (|| -> Result<Option<Option<RunMatch>>> {
                budget.check_run(index)?;
                let spans = match self.read_candidate(candidate, deadline)? {
                    Read::Spans(spans) => spans,
                    Read::Gone => return Ok(Some(None)),
                    Read::Unreadable => return Ok(None),
                };
                let read = self.read_steps(&spans, deadline)?;
                if options
                    .agent
                    .as_deref()
                    .is_some_and(|agent| read.agent.as_deref() != Some(agent))
                {
                    return Ok(Some(None));
                }
                self.match_run(
                    query,
                    &spans,
                    &read,
                    &candidate.run,
                    &candidate.tenant,
                    options,
                    1,
                    OutcomeOf::At(now_ns),
                    deadline,
                )
                .map(Some)
            })();
            match outcome {
                Ok(found) => {
                    result.runs_scanned = index + 1;
                    match found {
                        Some(Some(found)) => result.matches.push(found),
                        Some(None) => {}
                        None => result.runs_skipped += 1,
                    }
                }
                Err(Error::DeadlineExceeded(_)) => {
                    result.stopped = Some(Stopped::Deadline);
                    result.truncated = true;
                    result.next_until_ns = Some(candidate.latest);
                    break;
                }
                Err(other) => return Err(other),
            }
        }
        if window.capped {
            result.truncated = true;
            // Every run the cap left out was last active no later than the
            // oldest run kept.
            if result.stopped.is_none() {
                result.next_until_ns = candidates.last().map(|candidate| candidate.latest);
            }
        }
        Ok(result)
    }

    /// Matches `query` against one read run; `None` when it does not match
    /// or the run's outcome is filtered out.
    #[allow(clippy::too_many_arguments)]
    fn match_run(
        &self,
        query: &Query,
        spans: &crate::run::RunSpans,
        read: &ReadRun<'_>,
        run: &str,
        tenant: &str,
        options: &SearchOptions,
        keep: usize,
        outcome: OutcomeOf,
        deadline: Option<Deadline>,
    ) -> Result<Option<RunMatch>> {
        let scan = query
            .program
            .scan(&read.steps, keep, MAX_OCCURRENCES_COUNTED, deadline)?;
        let Some(first) = scan.found.first() else {
            return Ok(None);
        };
        let outcome = match outcome {
            OutcomeOf::Known(outcome) => outcome,
            OutcomeOf::At(now_ns) => run_outcome(spans, now_ns),
        };
        if !options.outcome.admits(outcome) {
            return Ok(None);
        }
        let first = occurrence(query, &read.steps, first);
        let occurrences = if options.run.is_some() {
            scan.found
                .iter()
                .map(|found| occurrence(query, &read.steps, found))
                .collect()
        } else {
            Vec::new()
        };
        Ok(Some(RunMatch {
            run: run.to_owned(),
            tenant: tenant.to_owned(),
            agent: read.agent.clone(),
            outcome,
            spans: first.spans,
            spans_truncated: first.spans_truncated,
            captures: first.captures,
            captures_truncated: first.captures_truncated,
            first_step: first.first_step,
            last_step: first.last_step,
            count: scan.count,
            count_capped: scan.count_capped,
            steps: read.steps.len(),
            last_activity_ns: spans
                .spans
                .iter()
                .map(|span| span.start_time_ns)
                .max()
                .unwrap_or(0),
            run_truncated: read.truncated,
            inexact: scan.inexact,
            occurrences,
        }))
    }
}

/// A run's outcome, computed already or to be computed (only for a run that
/// matches) as of a wall-clock instant.
#[derive(Clone, Copy)]
enum OutcomeOf {
    Known(Outcome),
    At(u64),
}

/// How a run ended: `unknown` for a run read past its span budget, since
/// an ending cannot be read from a beginning.
fn run_outcome(spans: &crate::run::RunSpans, now_ns: u64) -> Outcome {
    if spans.truncated {
        return Outcome::Unknown;
    }
    attribution::session_outcome(&spans.spans, now_ns, crate::mcp::SESSION_IDLE_NS).outcome
}

fn occurrence(query: &Query, steps: &[step::Step<'_>], found: &nfa::Found) -> Occurrence {
    let spans: Vec<MatchSpan> = found
        .path
        .iter()
        .take(MAX_MATCH_SPANS)
        .map(|&index| {
            let step = &steps[index];
            MatchSpan {
                trace_id: step.span.trace_id.clone(),
                span_id: step.span.span_id.clone(),
                step_index: index,
                kind: step.facts.kind,
                name: step.name().to_owned(),
            }
        })
        .collect();
    let mut captures = Map::new();
    let mut captures_truncated = false;
    for (slot, value) in found.env.slots().iter().enumerate() {
        if let Some(value) = value {
            let (json, cut) = value.to_json_capped(MAX_CAPTURE_BYTES);
            captures_truncated |= cut;
            captures.insert(query.captures[slot].clone(), json);
        }
    }
    Occurrence {
        first_step: found.start,
        last_step: found.end,
        spans_truncated: found.path.len() > MAX_MATCH_SPANS,
        spans,
        captures,
        captures_truncated,
    }
}

// ---------------------------------------------------------------- findings

/// What the findings scan reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindingsOptions {
    /// Window start, default [`DEFAULT_WINDOW_NS`] ago.
    pub since_ns: Option<u64>,
    /// Window end, default unbounded.
    pub until_ns: Option<u64>,
    /// Only runs of this agent.
    pub agent: Option<String>,
    /// Only these detectors (empty: the whole library).
    pub detectors: Vec<String>,
}

/// Runs by how they ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OutcomeCounts {
    /// Succeeded.
    pub success: usize,
    /// Failed.
    pub failure: usize,
    /// Abandoned.
    pub abandoned: usize,
    /// Still active or not determinable.
    pub unknown: usize,
}

impl OutcomeCounts {
    fn add(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Success => self.success += 1,
            Outcome::Failure => self.failure += 1,
            Outcome::Abandoned => self.abandoned += 1,
            Outcome::Unknown => self.unknown += 1,
        }
    }

    /// Successes among runs whose outcome is known; `None` when none is.
    pub fn success_rate(&self) -> Option<f64> {
        let decided = self.success + self.failure + self.abandoned;
        (decided > 0).then(|| self.success as f64 / decided as f64)
    }
}

/// One detector's findings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DetectorFindings {
    /// The detector.
    pub name: &'static str,
    /// Its title.
    pub title: &'static str,
    /// Its severity.
    pub severity: Severity,
    /// What it finds.
    pub summary: &'static str,
    /// Its pattern.
    pub query: &'static str,
    /// Runs it matched.
    pub runs_matched: usize,
    /// Occurrences across those runs.
    pub occurrences: usize,
    /// How the matched runs ended.
    pub outcomes: OutcomeCounts,
    /// Their success rate (`None` when no matched run's outcome is known).
    pub success_rate: Option<f64>,
    /// The most recent matched runs, at most [`MAX_DETECTOR_EXAMPLES`].
    pub examples: Vec<RunMatch>,
    /// The latest activity among matched runs.
    pub last_seen_ns: Option<u64>,
}

/// The detector library run over a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Findings {
    /// Per detector: those with findings first, by severity, then by runs
    /// matched.
    pub detectors: Vec<DetectorFindings>,
    /// Candidate runs examined, newest first (see
    /// [`SearchResult::runs_scanned`]): those the `agent` filter left out,
    /// and those gone or unreadable when reached, included.
    pub runs_scanned: usize,
    /// Runs the detectors were run on: the scanned runs that were read and
    /// passed the filters. The baseline's total, and the denominator to
    /// compare a detector's `runs_matched` against.
    pub runs_checked: usize,
    /// How every checked run ended — the base rate to compare a detector's
    /// matched runs against.
    pub baseline: OutcomeCounts,
    /// The baseline's success rate.
    pub baseline_success_rate: Option<f64>,
    /// Candidate runs in the window (at most [`MAX_FINDINGS_RUNS`]).
    pub runs_in_window: usize,
    /// Whether runs in the window went unchecked.
    pub truncated: bool,
    /// Why the scan stopped early, if it did.
    pub stopped: Option<Stopped>,
    /// Whether the window held more runs than [`MAX_FINDINGS_RUNS`].
    pub window_capped: bool,
    /// When runs went unchecked: where to continue from (see
    /// [`SearchResult::next_until_ns`]).
    pub next_until_ns: Option<u64>,
    /// Spans left out because their run id cannot be read back to them
    /// (see [`SearchResult::spans_skipped`]).
    pub spans_skipped: usize,
    /// Candidate runs left out because reading them by id was refused.
    pub runs_skipped: usize,
    /// The window's start.
    pub since_ns: u64,
    /// The window's end, when bounded.
    pub until_ns: Option<u64>,
}

impl Store {
    /// Runs the detector library (or the detectors named) over the most
    /// recent runs of a window, under the scoping and budget rules of
    /// [`Self::trajectory_search`].
    ///
    /// An unknown detector name is [`Error::Invalid`].
    pub fn detector_findings(
        &self,
        tenant: Option<&str>,
        options: &FindingsOptions,
        now_ns: u64,
    ) -> Result<Findings> {
        let deadline = self.query_deadline(Instant::now());
        self.detector_findings_under(
            tenant,
            options,
            now_ns,
            Budget::request(deadline, MAX_FINDINGS_RUNS),
        )
    }

    /// [`Self::detector_findings`] under explicit bounds.
    fn detector_findings_under(
        &self,
        tenant: Option<&str>,
        options: &FindingsOptions,
        now_ns: u64,
        budget: Budget,
    ) -> Result<Findings> {
        let deadline = budget.scan;
        let compiled = detectors::compiled();
        let mut selected: Vec<usize> = Vec::new();
        for name in &options.detectors {
            match DETECTORS.iter().position(|detector| detector.name == name) {
                Some(index) => {
                    if !selected.contains(&index) {
                        selected.push(index);
                    }
                }
                None => {
                    return Err(Error::Invalid(format!(
                        "unknown detector `{name}`; detectors are: {}",
                        DETECTORS
                            .iter()
                            .map(|detector| detector.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                }
            }
        }
        if selected.is_empty() {
            selected = (0..DETECTORS.len()).collect();
        }
        let since = options
            .since_ns
            .unwrap_or_else(|| now_ns.saturating_sub(DEFAULT_WINDOW_NS));
        let window = self.window_runs(
            tenant,
            since,
            options.until_ns,
            budget.window_cap,
            budget.enumerate,
        )?;
        let candidates = &window.candidates;
        let mut rows: Vec<DetectorFindings> = selected
            .iter()
            .map(|&index| {
                let detector = &DETECTORS[index];
                DetectorFindings {
                    name: detector.name,
                    title: detector.title,
                    severity: detector.severity,
                    summary: detector.summary,
                    query: detector.query,
                    runs_matched: 0,
                    occurrences: 0,
                    outcomes: OutcomeCounts::default(),
                    success_rate: None,
                    examples: Vec::new(),
                    last_seen_ns: None,
                }
            })
            .collect();
        let mut baseline = OutcomeCounts::default();
        let mut scanned = 0;
        let mut runs_skipped = 0;
        let mut stopped = None;
        let mut next_until_ns = None;
        let search_options = SearchOptions::default();
        for (index, candidate) in candidates.iter().enumerate() {
            let checked = (|| -> Result<Option<bool>> {
                budget.check_run(index)?;
                let spans = match self.read_candidate(candidate, deadline)? {
                    Read::Spans(spans) => spans,
                    Read::Gone => return Ok(Some(false)),
                    Read::Unreadable => return Ok(None),
                };
                let read = self.read_steps(&spans, deadline)?;
                if options
                    .agent
                    .as_deref()
                    .is_some_and(|agent| read.agent.as_deref() != Some(agent))
                {
                    return Ok(Some(false));
                }
                let outcome = run_outcome(&spans, now_ns);
                // Every detector is evaluated before any is recorded, so a
                // run the budget interrupts counts toward none of them.
                let mut hits = Vec::new();
                for (row, &detector) in selected.iter().enumerate() {
                    if let Some(found) = self.match_run(
                        &compiled[detector],
                        &spans,
                        &read,
                        &candidate.run,
                        &candidate.tenant,
                        &search_options,
                        1,
                        OutcomeOf::Known(outcome),
                        deadline,
                    )? {
                        hits.push((row, found));
                    }
                }
                baseline.add(outcome);
                for (row, found) in hits {
                    let entry = &mut rows[row];
                    entry.runs_matched += 1;
                    entry.occurrences += found.count;
                    entry.outcomes.add(found.outcome);
                    entry.last_seen_ns =
                        Some(entry.last_seen_ns.map_or(found.last_activity_ns, |seen| {
                            seen.max(found.last_activity_ns)
                        }));
                    if entry.examples.len() < MAX_DETECTOR_EXAMPLES {
                        entry.examples.push(found);
                    }
                }
                Ok(Some(true))
            })();
            match checked {
                Ok(read) => {
                    scanned = index + 1;
                    if read.is_none() {
                        runs_skipped += 1;
                    }
                }
                Err(Error::DeadlineExceeded(_)) => {
                    stopped = Some(Stopped::Deadline);
                    next_until_ns = Some(candidate.latest);
                    break;
                }
                Err(other) => return Err(other),
            }
        }
        if window.capped && stopped.is_none() {
            // As for a search: every run the cap left out was last active no
            // later than the oldest run kept.
            next_until_ns = candidates.last().map(|candidate| candidate.latest);
        }
        let runs_checked =
            baseline.success + baseline.failure + baseline.abandoned + baseline.unknown;
        for row in &mut rows {
            row.success_rate = row.outcomes.success_rate();
        }
        let library_rank = |name: &str| DETECTORS.iter().position(|d| d.name == name);
        rows.sort_by(|left, right| {
            (left.runs_matched == 0)
                .cmp(&(right.runs_matched == 0))
                .then(left.severity.cmp(&right.severity))
                .then(right.runs_matched.cmp(&left.runs_matched))
                .then(library_rank(left.name).cmp(&library_rank(right.name)))
        });
        Ok(Findings {
            detectors: rows,
            runs_scanned: scanned,
            runs_checked,
            baseline_success_rate: baseline.success_rate(),
            baseline,
            runs_in_window: candidates.len(),
            truncated: stopped.is_some() || window.capped,
            stopped,
            window_capped: window.capped,
            next_until_ns,
            spans_skipped: window.spans_skipped,
            runs_skipped,
            since_ns: since,
            until_ns: options.until_ns,
        })
    }
}

/// The detector library as JSON, for `GET /v1/detectors`.
pub fn library() -> Value {
    json!({
        "detectors": DETECTORS
            .iter()
            .map(|detector| serde_json::to_value(detector).unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::TempStore;

    const NOW: u64 = 1_800_000_000_000_000_000;
    const SECOND: u64 = 1_000_000_000;

    fn compile_error(text: &str) -> TqlError {
        match Query::compile(text) {
            Err(error) => error,
            Ok(_) => panic!("`{text}` must be refused"),
        }
    }

    #[test]
    fn parse_errors_name_the_column() {
        for (text, column, needle) in [
            ("", 1, "empty"),
            ("tool(bash) llm", 12, "join steps with `->`"),
            ("tools(bash)", 1, "did you mean `tool`"),
            ("tool(bash)[stauts=error]", 12, "did you mean `status`"),
            ("tool(bash)[status=error", 24, "expected `]`"),
            ("tool(a) ..", 11, "expected `>`"),
            ("tool(a) .. llm", 12, "expected `>`"),
            ("tool[cost~\"x\"]", 10, "is a number"),
            ("tool[duration>30]", 15, "give `duration` a unit"),
            ("tool[cost>30s]", 11, "duration compares with `duration`"),
            ("tool[args~\"(ab\"]", 12, "in regex: unclosed"),
            ("tool[args~/a{9999}/]", 13, "in regex"),
            (
                "tool[file=@f] ..> tool[file!=@g]",
                30,
                "`@g` is never bound",
            ),
            (
                "tool[error or file=@f] -> tool[file!=@f]",
                20,
                "never bound",
            ),
            ("tool?", 1, "can match zero steps"),
            ("(tool?)+", 8, "can match zero steps"),
            ("tool++", 6, "cannot follow another"),
            ("tool{0}", 5, "zero times"),
            ("tool{2,1}", 5, "below the minimum"),
            ("tool{101}", 6, "capped at 100"),
            ("tool within", 12, "takes a step count"),
            ("tool!x", 5, "write negation"),
            ("tool(4o)", 7, "unknown unit `o`"),
            ("tool[error]]", 12, "unexpected `]`"),
            ("^", 2, "the query ends where a step was expected"),
            ("tool -> $", 9, "`$` may only end"),
            ("test(x)", 5, "`test` takes no name"),
            ("tool[status>3]", 12, "is text"),
            ("tool[error=3]", 12, "true or false"),
            ("tool[\"x\"]", 6, "expected a field"),
            ("tool[attr.]", 6, "ends with `.`"),
            ("tool[name.x=1]", 6, "has no sub-fields"),
            ("(..> tool)", 1, "continues the step before it"),
            ("tool (..> llm | agent)", 17, "starts with `->` or `..>`"),
        ] {
            let error = compile_error(text);
            assert!(
                error.message.contains(needle),
                "`{text}`: expected {needle:?} in {error}"
            );
            assert_eq!(error.column, column, "`{text}`: {error}");
        }
    }

    #[test]
    fn a_query_holds_a_bounded_amount_of_regex() {
        // Each regex compiles to about 4,080 instructions: two fit the
        // query's budget, a third is refused at its own column.
        let heavy = r#""(?:[^\x00]{255}){16}""#;
        let two = format!("tool[args~{heavy} or result~{heavy}]");
        Query::compile(&two).expect("two fit");
        let three = format!("tool[args~{heavy} or result~{heavy} or span~{heavy}]");
        let error = compile_error(&three);
        assert!(error.message.contains("8192 instructions"), "{error}");
        let third = three.rfind("~\"").expect("third regex") + 3;
        assert_eq!(error.column, third, "{error}");
        // Regexes in names count too.
        let names = format!("tool(~{heavy}|~{heavy}|~{heavy})");
        assert!(compile_error(&names).message.contains("in all"));
    }

    #[test]
    fn the_pointed_error_puts_the_caret_under_the_column() {
        let error = compile_error("tool(bash)[stauts=error]");
        let pointed = error.pointed("tool(bash)[stauts=error]");
        let lines: Vec<&str> = pointed.lines().collect();
        assert_eq!(lines[1].find('^'), Some(11));
    }

    #[test]
    fn normalized_rendering_shows_precedence() {
        for (text, rendered) in [
            ("tool(a) -> tool(b) | llm", "tool(a) -> tool(b) | llm"),
            ("tool(a) -> (tool(b) | llm)", "tool(a) -> (tool(b) | llm)"),
            ("TOOL( bash )[ status = error ]", "tool(bash)[status=error]"),
            ("tool[a.b=1]", ""),
            ("^tool..>llm$", "^tool ..> llm$"),
            (
                "tool[args~/pytest/i, cost > 0.5]",
                "tool[args~\"(?i)pytest\" and cost>0.5]",
            ),
            (
                "llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6",
                "llm[context>=1000 and context=@c] ..> llm[context>=2*@c] within 6",
            ),
            (
                "tool[duration>90s] within 5m",
                "tool[duration>90s] within 5m",
            ),
            (
                "tool[attr.http.status_code>=500]",
                "tool[attr.http.status_code>=500]",
            ),
            ("tool[attr.\"weird key\"=1]", "tool[attr.\"weird key\"=1]"),
            ("tool(~\"^mcp__\"|Bash)", "tool(~\"^mcp__\"|Bash)"),
            ("tool(/^mcp__/)", "tool(~\"^mcp__\")"),
            (
                "tool ..[kind=llm]{1,3}> tool",
                "tool ..[kind=llm]{1,3}> tool",
            ),
            ("tool(a){0,1} -> llm", "tool(a)? -> llm"),
            (
                "tool(@t)[error] (..[not name=@t]> tool(@t)[error]){2,} ..> llm$",
                "tool(@t)[error] (..[not name=@t]> tool(@t)[error]){2,} ..> llm$",
            ),
            ("tool (-> llm | ..> agent)+", "tool (-> llm | ..> agent)+"),
            ("tool (-> llm -> tool)", "tool -> (llm -> tool)"),
            ("tool (..> llm)", "tool (..> llm)"),
            ("(tool -> llm){2,}?", "(tool -> llm){2,}?"),
            (
                "tool[not (error or status=ok)]",
                "tool[not (error or status=ok)]",
            ),
            ("tool[file=\"src/main.rs\"]", "tool[file=src/main.rs]"),
            ("tool[status=\"and\"]", "tool[status=\"and\"]"),
        ] {
            match Query::compile(text) {
                Ok(query) => assert_eq!(query.normalized(), rendered, "`{text}`"),
                Err(error) => assert!(rendered.is_empty(), "`{text}`: {error}"),
            }
        }
    }

    #[test]
    fn explain_reports_shape() {
        let query =
            Query::compile("edit[file=@f] (..> edit[file=@f]){4,} within 10").expect("compiles");
        let explain = query.explain();
        assert_eq!(explain.captures, ["f"]);
        assert_eq!(explain.fields, ["file"]);
        assert_eq!(explain.min_steps, 5);
        assert_eq!(explain.max_steps, None);
        assert_eq!(explain.within, Some(json!({"steps": 10})));
        assert_eq!(explain.atoms, 2);
    }

    fn span(
        tenant: &str,
        session: &str,
        id: &str,
        offset: u64,
        status: &str,
        attributes: Value,
    ) -> crate::Span {
        let mut attributes = attributes;
        attributes["session.id"] = json!(session);
        crate::Span {
            trace_id: format!("tr-{session}"),
            span_id: id.into(),
            tenant: tenant.into(),
            parent_span_id: None,
            name: id.into(),
            start_time_ns: NOW - 3_600 * SECOND + offset * SECOND,
            end_time_ns: NOW - 3_600 * SECOND + offset * SECOND + SECOND / 2,
            status: status.into(),
            service: "coder".into(),
            attributes: attributes.as_object().expect("object").clone(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn bash(
        tenant: &str,
        session: &str,
        id: &str,
        offset: u64,
        command: &str,
        status: &str,
    ) -> crate::Span {
        span(
            tenant,
            session,
            id,
            offset,
            status,
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Bash",
                   "gen_ai.tool.call.arguments": {"command": command}}),
        )
    }

    fn edit(tenant: &str, session: &str, id: &str, offset: u64, file: &str) -> crate::Span {
        span(
            tenant,
            session,
            id,
            offset,
            "ok",
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Edit",
                   "gen_ai.tool.call.arguments": {"file_path": file, "old_string": "a", "new_string": "b"}}),
        )
    }

    fn seeded(label: &str) -> TempStore {
        let fixture = TempStore::open(label, crate::Config::default());
        let mut spans = vec![
            // `tampered` (oldest): fails, edits the test, passes.
            bash("", "tampered", "a", 0, "pytest", "error"),
            edit("", "tampered", "b", 1, "tests/test_x.py"),
            bash("", "tampered", "c", 2, "pytest", "ok"),
            // `honest`: fails, fixes the code, passes.
            bash("", "honest", "a", 10, "pytest", "error"),
            edit("", "honest", "b", 11, "src/x.py"),
            bash("", "honest", "c", 12, "pytest", "ok"),
        ];
        // `failed` (newest): fails three times and ends on an error.
        for (index, offset) in [20_u64, 21, 22].iter().enumerate() {
            spans.push(bash(
                "",
                "failed",
                &format!("f{index}"),
                *offset,
                "pytest",
                "error",
            ));
        }
        // Another tenant's tampering run, newest of all.
        spans.push(bash("acme", "acme-run", "a", 30, "pytest", "error"));
        spans.push(edit("acme", "acme-run", "b", 31, "tests/test_y.py"));
        spans.push(bash("acme", "acme-run", "c", 32, "pytest", "ok"));
        fixture.store().ingest_batch(spans).expect("ingests");
        fixture
    }

    #[test]
    fn search_scans_recent_runs_first_and_scopes_tenants() {
        let fixture = seeded("search");
        let store = fixture.store();
        let query = Query::compile("test[error] ..> edit[file~\"test\"] ..> test[not error]")
            .expect("compiles");
        let result = store
            .trajectory_search(Some(""), &query, &SearchOptions::default(), NOW)
            .expect("searches");
        assert_eq!(result.runs_scanned, 3);
        assert_eq!(result.matches.len(), 1);
        let found = &result.matches[0];
        assert_eq!(found.run, "tampered");
        assert_eq!(found.outcome, Outcome::Success);
        assert_eq!(found.agent.as_deref(), Some("coder"));
        let ids: Vec<_> = found
            .spans
            .iter()
            .map(|span| span.span_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(!result.truncated);

        // The operator view sees both tenants, newest first.
        let all = store
            .trajectory_search(None, &query, &SearchOptions::default(), NOW)
            .expect("searches");
        let runs: Vec<_> = all
            .matches
            .iter()
            .map(|m| (m.tenant.as_str(), m.run.as_str()))
            .collect();
        assert_eq!(runs, [("acme", "acme-run"), ("", "tampered")]);

        // A bound tenant sees only its own.
        let acme = store
            .trajectory_search(Some("acme"), &query, &SearchOptions::default(), NOW)
            .expect("searches");
        assert_eq!(acme.matches.len(), 1);
        assert_eq!(acme.matches[0].run, "acme-run");
        assert_eq!(acme.runs_scanned, 1);
    }

    #[test]
    fn limit_outcome_window_and_single_run() {
        let fixture = seeded("options");
        let store = fixture.store();
        let any_test = Query::compile("test").expect("compiles");
        let limited = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    limit: 1,
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        assert_eq!(limited.matches.len(), 1);
        assert_eq!(limited.matches[0].run, "failed", "most recent first");
        assert_eq!(limited.stopped, Some(Stopped::Limit));
        assert!(limited.truncated);
        // Continuing from the resume point reaches the older runs.
        let next = limited.next_until_ns.expect("a resume point");
        assert_eq!(
            next,
            NOW - 3_600 * SECOND + 12 * SECOND,
            "the newest unchecked run"
        );
        let rest = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    until_ns: Some(next),
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        let runs: Vec<_> = rest.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["honest", "tampered"]);

        let failures = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    outcome: OutcomeFilter::Failure,
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        let runs: Vec<_> = failures.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["failed"]);
        assert_eq!(
            failures.matches[0].count, 3,
            "three non-overlapping test steps"
        );

        // A window ending before the newest run leaves it out.
        let windowed = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    until_ns: Some(NOW - 3_600 * SECOND + 15 * SECOND),
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        let runs: Vec<_> = windowed.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["honest", "tampered"]);

        let single = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    run: Some("failed".into()),
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        assert_eq!(single.matches[0].occurrences.len(), 3);
        assert_eq!(single.since_ns, None);

        let agent = store
            .trajectory_search(
                Some(""),
                &any_test,
                &SearchOptions {
                    agent: Some("someone-else".into()),
                    ..SearchOptions::default()
                },
                NOW,
            )
            .expect("searches");
        assert!(agent.matches.is_empty());
        assert_eq!(agent.runs_scanned, 3);
    }

    #[test]
    fn findings_count_runs_and_compare_outcomes() {
        let fixture = seeded("findings");
        let store = fixture.store();
        let findings = store
            .detector_findings(Some(""), &FindingsOptions::default(), NOW)
            .expect("finds");
        assert_eq!(findings.runs_scanned, 3);
        assert_eq!(findings.baseline.success, 2);
        assert_eq!(findings.baseline.failure, 1);
        let row = |name: &str| {
            findings
                .detectors
                .iter()
                .find(|row| row.name == name)
                .expect("row")
        };
        let tampering = row("test_tampering");
        assert_eq!(tampering.runs_matched, 1);
        assert_eq!(tampering.examples[0].run, "tampered");
        assert_eq!(tampering.success_rate, Some(1.0));
        assert_eq!(
            findings.detectors[0].name, "test_tampering",
            "high severity with findings first"
        );
        assert_eq!(row("hallucinated_tool").runs_matched, 0);

        let one = store
            .detector_findings(
                Some(""),
                &FindingsOptions {
                    detectors: vec!["edit_thrash".into()],
                    ..FindingsOptions::default()
                },
                NOW,
            )
            .expect("finds");
        assert_eq!(one.detectors.len(), 1);
        match store.detector_findings(
            Some(""),
            &FindingsOptions {
                detectors: vec!["nope".into()],
                ..FindingsOptions::default()
            },
            NOW,
        ) {
            Err(Error::Invalid(message)) => assert!(message.contains("unknown detector")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_window_keeps_exactly_the_most_recent_runs() {
        let fixture = TempStore::open("window", crate::Config::default());
        let store = fixture.store();
        // Runs whose spans arrive out of order, so eviction and re-admission
        // are both exercised.
        let mut spans = Vec::new();
        for run in 0..40_u64 {
            let session = format!("r{run:02}");
            let offset = (run * 7) % 40;
            spans.push(bash("", &session, "a", offset, "ls", "ok"));
            spans.push(bash("", &session, "b", offset + 50, "ls", "ok"));
        }
        store.ingest_batch(spans).expect("ingests");
        let Window {
            candidates, capped, ..
        } = store
            .window_runs(Some(""), 0, None, 10, None)
            .expect("enumerates");
        assert!(capped);
        let mut expected: Vec<(u64, String)> = (0..40_u64)
            .map(|run| ((run * 7) % 40 + 50, format!("r{run:02}")))
            .collect();
        expected.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let wanted: Vec<_> = expected
            .iter()
            .take(10)
            .map(|(_, run)| run.clone())
            .collect();
        let got: Vec<_> = candidates.iter().map(|c| c.run.clone()).collect();
        assert_eq!(got, wanted);
    }

    fn unbounded(window_cap: usize) -> Budget {
        Budget::request(None, window_cap)
    }

    #[test]
    fn a_budget_spent_mid_scan_returns_what_it_found_and_says_so() {
        let fixture = seeded("scan-deadline");
        let store = fixture.store();
        // The budget runs out after the newest run (`failed`) is checked.
        let partway = Budget {
            scan_runs: Some(1),
            ..unbounded(MAX_SEARCH_RUNS)
        };
        let query = Query::compile("test").expect("compiles");
        let result = store
            .trajectory_search_under(Some(""), &query, &SearchOptions::default(), NOW, partway)
            .expect("answers with what it found");
        assert_eq!(result.stopped, Some(Stopped::Deadline));
        assert!(result.truncated);
        assert_eq!(result.runs_in_window, 3);
        assert_eq!(result.runs_scanned, 1);
        let runs: Vec<_> = result.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["failed"], "what was found before the stop is kept");
        assert_eq!(
            result.next_until_ns,
            Some(NOW - 3_600 * SECOND + 12 * SECOND),
            "the first unchecked run: `honest`"
        );

        let findings = store
            .detector_findings_under(Some(""), &FindingsOptions::default(), NOW, partway)
            .expect("answers with what it found");
        assert_eq!(findings.stopped, Some(Stopped::Deadline));
        assert!(findings.truncated);
        assert_eq!(findings.runs_scanned, 1);
        assert_eq!(findings.runs_checked, 1);
        assert_eq!(
            findings.baseline,
            OutcomeCounts {
                failure: 1,
                ..OutcomeCounts::default()
            }
        );
        assert!(
            findings.detectors.iter().any(|row| row.runs_matched > 0),
            "the checked run's findings are kept"
        );
        for row in &findings.detectors {
            assert!(
                row.examples.iter().all(|example| example.run == "failed"),
                "{}: only the checked run can have matched",
                row.name
            );
        }
        assert_eq!(
            findings.next_until_ns,
            Some(NOW - 3_600 * SECOND + 12 * SECOND)
        );

        // A deadline already spent stops the scan before its first run.
        let expired = Budget {
            scan: Deadline::starting(Instant::now(), Some(std::time::Duration::ZERO)),
            ..unbounded(MAX_SEARCH_RUNS)
        };
        let none = store
            .trajectory_search_under(Some(""), &query, &SearchOptions::default(), NOW, expired)
            .expect("answers");
        assert_eq!((none.runs_scanned, none.matches.len()), (0, 0));
        assert_eq!(none.stopped, Some(Stopped::Deadline));

        // With budget to spare the same scans complete.
        let complete = store
            .trajectory_search_under(
                Some(""),
                &query,
                &SearchOptions::default(),
                NOW,
                unbounded(MAX_SEARCH_RUNS),
            )
            .expect("searches");
        assert_eq!(complete.stopped, None);
        assert_eq!(complete.runs_scanned, 3);
        assert_eq!(complete.next_until_ns, None);
    }

    #[test]
    fn a_capped_window_says_where_to_continue() {
        let fixture = seeded("capped-window");
        let store = fixture.store();
        let query = Query::compile("test").expect("compiles");
        // Two of the three runs are considered: `failed` and `honest`.
        let capped = store
            .trajectory_search_under(
                Some(""),
                &query,
                &SearchOptions::default(),
                NOW,
                unbounded(2),
            )
            .expect("searches");
        assert!(capped.window_capped && capped.truncated);
        assert_eq!(capped.stopped, None);
        let runs: Vec<_> = capped.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["failed", "honest"]);
        let next = capped.next_until_ns.expect("a resume point");
        assert_eq!(
            next,
            NOW - 3_600 * SECOND + 12 * SECOND,
            "the oldest kept run"
        );
        // Continuing reaches the run the cap left out.
        let rest = store
            .trajectory_search_under(
                Some(""),
                &query,
                &SearchOptions {
                    until_ns: Some(next),
                    ..SearchOptions::default()
                },
                NOW,
                unbounded(2),
            )
            .expect("searches");
        let runs: Vec<_> = rest.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["honest", "tampered"]);
        assert!(!rest.window_capped);
        assert_eq!(rest.next_until_ns, None);

        let findings = store
            .detector_findings_under(Some(""), &FindingsOptions::default(), NOW, unbounded(2))
            .expect("finds");
        assert!(findings.window_capped);
        assert_eq!(findings.next_until_ns, Some(next));
    }

    fn session_span(session: &str, trace: &str, id: &str, offset: u64) -> crate::Span {
        let mut span = bash("", session, id, offset, "pytest", "ok");
        span.trace_id = trace.into();
        span
    }

    #[test]
    fn a_run_id_that_cannot_be_read_back_is_skipped_not_fatal() {
        let fixture = TempStore::open("poisoned", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                session_span("good", "t-good", "a", 0),
                // `trace:` names no trace at all.
                session_span("trace:", "t-poison", "b", 1),
                // A session claiming another trace's run id.
                session_span("trace:t-good", "t-other", "c", 2),
                // A session naming its own trace reads back fine.
                session_span("trace:t-self", "t-self", "d", 3),
            ])
            .expect("ingests");
        let query = Query::compile("test").expect("compiles");
        let result = store
            .trajectory_search(Some(""), &query, &SearchOptions::default(), NOW)
            .expect("one bad span does not fail the search");
        let mut runs: Vec<_> = result.matches.iter().map(|m| m.run.as_str()).collect();
        runs.sort_unstable();
        assert_eq!(runs, ["good", "trace:t-self"]);
        assert_eq!(result.spans_skipped, 2);
        assert_eq!(result.runs_skipped, 0);
        let findings = store
            .detector_findings(Some(""), &FindingsOptions::default(), NOW)
            .expect("nor the findings");
        assert_eq!(findings.spans_skipped, 2);
        assert_eq!(findings.runs_checked, 2);

        // An id that is refused when read is skipped and counted.
        let unreadable = Candidate {
            tenant: String::new(),
            run: "trace:".into(),
            latest: 0,
        };
        assert!(matches!(
            store.read_candidate(&unreadable, None),
            Ok(Read::Unreadable)
        ));
        let gone = Candidate {
            run: "no-such-run".into(),
            ..unreadable
        };
        assert!(matches!(store.read_candidate(&gone, None), Ok(Read::Gone)));
    }

    #[test]
    fn findings_name_the_runs_they_checked() {
        let fixture = seeded("checked");
        let store = fixture.store();
        let mut spans = Vec::new();
        let mut other = bash("", "other-agent", "a", 40, "pytest", "ok");
        other.service = "reviewer".into();
        spans.push(other);
        store.ingest_batch(spans).expect("ingests");
        let findings = store
            .detector_findings(
                Some(""),
                &FindingsOptions {
                    agent: Some("coder".into()),
                    ..FindingsOptions::default()
                },
                NOW,
            )
            .expect("finds");
        // Four runs read; the reviewer's left out by the agent filter.
        assert_eq!(findings.runs_scanned, 4);
        assert_eq!(findings.runs_checked, 3);
        assert_eq!(
            findings.runs_checked,
            findings.baseline.success
                + findings.baseline.failure
                + findings.baseline.abandoned
                + findings.baseline.unknown
        );
    }

    #[test]
    fn captured_values_are_cut_in_the_answer_not_in_the_match() {
        let fixture = TempStore::open("captures", crate::Config::default());
        let store = fixture.store();
        let long = format!("pytest {}", "é".repeat(MAX_CAPTURE_BYTES));
        store
            .ingest_batch(vec![
                bash("", "s", "a", 0, &long, "error"),
                bash("", "s", "b", 1, &long, "ok"),
                bash("", "s", "c", 2, "pytest -q", "ok"),
            ])
            .expect("ingests");
        let query = Query::compile("tool[args=@a] -> tool[args=@a]").expect("compiles");
        let result = store
            .trajectory_search(Some(""), &query, &SearchOptions::default(), NOW)
            .expect("searches");
        // The whole value matched (a and b, not b and c) ...
        let found = &result.matches[0];
        assert_eq!((found.first_step, found.last_step), (0, 1));
        // ... and the report carries its first KiB, flagged.
        let captured = found.captures["a"].as_str().expect("text");
        assert!(captured.len() <= MAX_CAPTURE_BYTES && long.starts_with(captured));
        assert!(found.captures_truncated);

        let short = Query::compile("tool[args=\"pytest -q\", args=@a]").expect("compiles");
        let result = store
            .trajectory_search(Some(""), &short, &SearchOptions::default(), NOW)
            .expect("searches");
        assert_eq!(result.matches[0].captures["a"], json!("pytest -q"));
        assert!(!result.matches[0].captures_truncated);
    }

    #[test]
    fn a_heavy_regex_stops_at_the_deadline_with_what_was_found() {
        let fixture = TempStore::open("heavy-regex", crate::Config::default());
        let store = fixture.store();
        let big = "a".repeat(64 * 1024);
        store
            .ingest_batch(vec![
                bash("", "light", "a", 10, "pytest", "ok"),
                bash("", "heavy", "a", 0, &big, "ok"),
            ])
            .expect("ingests");
        // Two regexes of about 4,000 live instructions on every one of
        // 65,536 characters: seconds of work each, even in a release build.
        let heavy = r#"(?:[^\x00]{255}){16}\x00"#;
        let query = Query::compile(&format!(
            "test | tool[args~\"{heavy}\" or args~\"{heavy}y\"]"
        ))
        .expect("compiles");
        let budget = Budget {
            scan: Deadline::starting(Instant::now(), Some(std::time::Duration::from_secs(1))),
            ..unbounded(MAX_SEARCH_RUNS)
        };
        let started = Instant::now();
        let result = store
            .trajectory_search_under(Some(""), &query, &SearchOptions::default(), NOW, budget)
            .expect("answers with what it found");
        let elapsed = started.elapsed();
        assert_eq!(result.stopped, Some(Stopped::Deadline));
        let runs: Vec<_> = result.matches.iter().map(|m| m.run.as_str()).collect();
        assert_eq!(runs, ["light"]);
        assert_eq!(result.runs_scanned, 1);
        assert_eq!(
            result.next_until_ns,
            Some(NOW - 3_600 * SECOND),
            "the heavy run is where to continue"
        );
        // The regexes alone would take several seconds; the scan stops
        // within a sliver of its one-second budget.
        assert!(
            elapsed < std::time::Duration::from_millis(2_500),
            "stopped after {elapsed:?}"
        );
    }

    #[test]
    fn an_exhausted_budget_stops_the_scan_and_says_so() {
        let config = crate::Config {
            query_deadline: Some(std::time::Duration::ZERO),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("deadline", config);
        let store = fixture.store();
        store
            .ingest_batch(vec![bash("", "s", "a", 0, "ls", "ok")])
            .expect("ingests");
        let query = Query::compile("tool").expect("compiles");
        // Enumeration refuses outright: there is nothing partial to show.
        match store.trajectory_search(Some(""), &query, &SearchOptions::default(), NOW) {
            Err(Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
    }
}
