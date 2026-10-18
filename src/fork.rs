//! Fork & Rewind: a recorded run branched at any step, and its branches found
//! again.
//!
//! A replay ([`crate::replay`]) re-executes an agent against its own
//! recording. A FORK is a replay that is told where the recording stops being
//! the truth:
//!
//! - **Edits** serve a different answer at one step — a model answer at an
//!   llm step (`{"step": 7, "response": …}`), or a tool result at a tool step
//!   (`{"step": 7, "tool_result": …}`) — in place of the recorded one. Strict
//!   matching keeps applying up to and including the first edited step (the
//!   agent still asks the recorded question there) and is off after it: the
//!   agent has seen something the recording never did, so its later requests
//!   cannot be expected to match.
//! - **A live point** (`"live_after": n`) ends the recording's authority:
//!   after step `n` every model call and every `tools/next` answers `409
//!   replay_live`, which is a harness's signal to send that call to the real
//!   provider (or run the real tool) instead. Steps after the live point are
//!   never held, so they need not even be replayable.
//!
//! The canonical fork is both: replay steps `0..n` exactly, serve an edited
//! answer at `n`, go live after it. A live point alone is REWIND — replay a
//! long run's beginning for free, then let the agent continue for real from
//! any step.
//!
//! A fork's creation response carries a `fork_id` and the attributes its
//! harness stamps on the spans the forked run records —
//! [`FORK_PARENT`], [`FORK_STEP`], [`FORK_ID`] — so that
//! `GET /v1/forks?run=` ([`Store::forks`]) can find every branch of a run
//! again and say, through the differ ([`crate::diff`]), where each one parted
//! from its parent and how it ended.
//!
//! The same module answers `GET /v1/goldens` ([`Store::goldens`]): the runs
//! marked as regression baselines — by a span attribute [`GOLDEN`] `= true`
//! recorded with the run, or after the fact by an annotation named
//! [`GOLDEN`] on the run's session or trace — which a test runner replays
//! strictly to prove an agent still takes the recorded path.
//!
//! Traza never calls a model: the live half of a fork is the harness's, not
//! the server's. Every read here is tenant-scoped, bounded (the caps below
//! are stated in the guide) and spends the request's one query budget.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use crate::annotations::AnnotationQuery;
use crate::attribution::Outcome;
use crate::diff::{Delta, FirstDivergence, RunSide, MAX_DIFF_SPANS, MAX_DIFF_STEPS};
use crate::semconv::{self, StepKind};
use crate::tape::{RunTape, TapeStep};
use crate::{Deadline, Error, Result, Span, SpanFilter, Store};

/// The span attribute naming the run a forked run branched from.
pub const FORK_PARENT: &str = "traza.fork.parent";

/// The span attribute naming the tape step a forked run was cut at: the
/// first step whose answer is not the parent's (see [`ForkSpec::cut`]).
pub const FORK_STEP: &str = "traza.fork.step";

/// The span attribute carrying the `fork_id` of the replay that served the
/// forked run's beginning.
pub const FORK_ID: &str = "traza.fork.id";

/// The span attribute, and the annotation name, that marks a run as a
/// regression baseline.
pub const GOLDEN: &str = "traza.golden";

/// Edits one fork may carry. A fork is a branch at a point, not a rewritten
/// recording; sixty-four is far more than a counterfactual needs, and keeps
/// the per-call bookkeeping trivially small.
pub const MAX_EDITS: usize = 64;

/// Spans carrying [`FORK_PARENT`] one listing examines. Past it the listing
/// is built from the earliest tagged spans and says it was truncated.
pub const MAX_FORK_SPANS: usize = 10_000;

/// Forks one listing returns when the caller names no limit.
pub const DEFAULT_FORK_LIMIT: usize = 20;

/// The most forks one listing returns. Each is read in full and diffed
/// against its parent, so the bound is on work as much as on size.
pub const MAX_FORK_LIMIT: usize = 50;

/// Spans carrying [`GOLDEN`] one golden listing examines.
pub const MAX_GOLDEN_SPANS: usize = 20_000;

/// Annotations named [`GOLDEN`] one golden listing examines, newest first.
pub const MAX_GOLDEN_ANNOTATIONS: usize = 10_000;

/// Golden runs one listing returns when the caller names no limit.
pub const DEFAULT_GOLDEN_LIMIT: usize = 200;

/// The most golden runs one listing returns.
pub const MAX_GOLDEN_LIMIT: usize = 1_000;

// ------------------------------------------------------------------- edits

/// What an edit serves in place of the recording.
#[derive(Clone, Debug, PartialEq)]
pub enum EditValue {
    /// A model answer, at an llm step: answer text (a string), one assistant
    /// message (OpenAI's `{"content", "tool_calls"}`, Anthropic's content
    /// blocks, or the normalised `{"role", "content", "tool_calls"}`), a list
    /// of them, or a whole OpenAI response body with `choices`.
    Response(Value),
    /// A tool result, at a tool step: any JSON, served by `tools/next` as
    /// the step's `result`.
    ToolResult(Value),
}

impl EditValue {
    /// The wire name of the edit's kind: `response` or `tool_result`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Response(_) => "response",
            Self::ToolResult(_) => "tool_result",
        }
    }
}

/// One edit: the tape step it replaces, and what is served there instead.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    /// The tape index of the step whose recorded answer is replaced.
    pub step: usize,
    /// What is served instead.
    pub value: EditValue,
}

/// How a replay is to be forked: its edits and its live point. The default —
/// no edits, no live point — is a plain replay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForkSpec {
    /// The edits, in ascending step order, at most one per step.
    pub edits: Vec<Edit>,
    /// After this tape step every call answers `replay_live`.
    pub live_after: Option<usize>,
}

impl ForkSpec {
    /// Reads a `POST /v1/replays` body's `edits` and `live_after` fields.
    ///
    /// Refuses, naming the problem: `edits` that is not a list, holds more
    /// than [`MAX_EDITS`] entries, or an entry that is not `{"step": n,
    /// "response": …}` or `{"step": n, "tool_result": …}` exactly; two edits
    /// of one step; a `live_after` that is not a non-negative integer; and an
    /// edit after the live point, which would never be served. Whether each
    /// step exists, and is of the edited kind, is the run's to say
    /// ([`Self::check_against`]).
    pub fn parse(
        edits: Option<&Value>,
        live_after: Option<&Value>,
    ) -> std::result::Result<Self, String> {
        let live_after = match live_after {
            None | Some(Value::Null) => None,
            Some(value) => Some(step_number(value).ok_or(
                "live_after must be a non-negative integer: the tape step after which every \
                 call answers replay_live",
            )?),
        };
        let mut parsed: Vec<Edit> = Vec::new();
        match edits {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                if items.len() > MAX_EDITS {
                    return Err(format!(
                        "a fork carries at most {MAX_EDITS} edits; this one has {}",
                        items.len()
                    ));
                }
                for (position, item) in items.iter().enumerate() {
                    parsed.push(parse_edit(position, item)?);
                }
            }
            Some(_) => {
                return Err(
                    "edits must be a list of {\"step\": n, \"response\": …} or {\"step\": n, \
                     \"tool_result\": …}"
                        .to_owned(),
                )
            }
        }
        parsed.sort_by_key(|edit| edit.step);
        if let Some(pair) = parsed.windows(2).find(|pair| pair[0].step == pair[1].step) {
            return Err(format!(
                "two edits name step {}; a step is served once, so it can be edited once",
                pair[0].step
            ));
        }
        if let (Some(cut), Some(late)) = (
            live_after,
            parsed
                .iter()
                .find(|edit| live_after.is_some_and(|cut| edit.step > cut)),
        ) {
            return Err(format!(
                "the edit at step {} comes after live_after {cut}: past the live point every \
                 call goes to the real provider, so it would never be served",
                late.step
            ));
        }
        Ok(Self {
            edits: parsed,
            live_after,
        })
    }

    /// Whether this is a fork at all, rather than a plain replay.
    pub fn is_fork(&self) -> bool {
        !self.edits.is_empty() || self.live_after.is_some()
    }

    /// The first edited step: strict matching applies up to and including
    /// it, and not after.
    pub fn first_edit(&self) -> Option<usize> {
        self.edits.iter().map(|edit| edit.step).min()
    }

    /// The step the fork is cut at — the first step whose answer is not the
    /// parent's: the first edited step, or the step after the live point,
    /// whichever comes first. `None` for a plain replay.
    pub fn cut(&self) -> Option<usize> {
        let after_live = self.live_after.map(|step| step.saturating_add(1));
        match (self.first_edit(), after_live) {
            (Some(edit), Some(live)) => Some(edit.min(live)),
            (edit, live) => edit.or(live),
        }
    }

    /// Checks the spec against the run's tape: every edited step exists and
    /// is of the edited kind, and the live point is a step of the run.
    pub(crate) fn check_against(&self, steps: &[TapeStep]) -> std::result::Result<(), String> {
        let last = steps.len().saturating_sub(1);
        if let Some(cut) = self.live_after {
            if cut >= steps.len() {
                return Err(format!(
                    "live_after {cut} is past the run's last step ({last}); the tape has {} \
                     steps",
                    steps.len()
                ));
            }
        }
        for edit in &self.edits {
            let Some(step) = steps.get(edit.step) else {
                return Err(format!(
                    "the edit at step {} is past the run's last step ({last})",
                    edit.step
                ));
            };
            let wanted = match edit.value {
                EditValue::Response(_) => StepKind::Llm,
                EditValue::ToolResult(_) => StepKind::Tool,
            };
            if step.kind != wanted {
                let remedy = match step.kind {
                    StepKind::Llm => "a model answer is edited with \"response\"",
                    StepKind::Tool => "a tool result is edited with \"tool_result\"",
                    _ => "only model answers and tool results can be edited",
                };
                return Err(format!(
                    "step {} is {} ({}), so a \"{}\" edit cannot replace it: {remedy}",
                    edit.step,
                    kind_phrase(step.kind),
                    step.detail.signature,
                    edit.value.kind()
                ));
            }
        }
        Ok(())
    }
}

/// `a model call`, `a tool call`, `an agent invocation`.
fn kind_phrase(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Llm => "a model call",
        StepKind::Tool => "a tool call",
        StepKind::Agent => "an agent invocation",
        _ => "not a step",
    }
}

/// A JSON value as a tape step number: a non-negative integer.
fn step_number(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|step| usize::try_from(step).ok())
}

fn parse_edit(position: usize, item: &Value) -> std::result::Result<Edit, String> {
    let shape = || {
        format!(
            "edit {position} must be {{\"step\": n, \"response\": …}} or {{\"step\": n, \
             \"tool_result\": …}}"
        )
    };
    let object = item.as_object().ok_or_else(shape)?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "step" | "response" | "tool_result"))
    {
        return Err(format!("edit {position} has an unknown field: {unknown}"));
    }
    let step = object
        .get("step")
        .and_then(step_number)
        .ok_or_else(|| format!("edit {position} needs \"step\": a non-negative integer"))?;
    let value = match (object.get("response"), object.get("tool_result")) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "edit {position} names both \"response\" and \"tool_result\"; a step is one or \
                 the other"
            ))
        }
        (Some(Value::Null), None) => {
            return Err(format!(
                "edit {position}'s \"response\" is null; an edited model answer needs text or \
                 a message"
            ))
        }
        (Some(response), None) => EditValue::Response(response.clone()),
        // A tool result of `null` is a result, served as recorded would be.
        (None, Some(result)) => EditValue::ToolResult(result.clone()),
        (None, None) => return Err(shape()),
    };
    Ok(Edit { step, value })
}

/// A fork as its creation response and status report it.
#[derive(Clone, Debug, PartialEq)]
pub struct ForkInfo {
    /// The fork id, `fk_<hex>`.
    pub id: String,
    /// The run it branches from.
    pub parent: String,
    /// The step it is cut at ([`ForkSpec::cut`]).
    pub step: usize,
    /// Each edit's step and kind, in step order.
    pub edits: Vec<(usize, &'static str)>,
    /// The live point, if any.
    pub live_after: Option<usize>,
}

impl ForkInfo {
    /// The span attributes a harness stamps on the forked run's spans.
    pub fn attributes(&self) -> Value {
        json!({
            FORK_PARENT: self.parent,
            FORK_STEP: self.step,
            FORK_ID: self.id,
        })
    }

    /// The `fork` object of a creation response.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "parent": self.parent,
            "step": self.step,
            "edits": self
                .edits
                .iter()
                .map(|(step, kind)| json!({"step": step, "kind": kind}))
                .collect::<Vec<_>>(),
            "live_after": self.live_after,
            "attributes": self.attributes(),
        })
    }
}

// ------------------------------------------------------------ fork listing

/// One fork of a run, as `GET /v1/forks` lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForkSummary {
    /// The forked run's id.
    pub run: String,
    /// Its tenant (`""` is the default tenant).
    pub tenant: String,
    /// The [`FORK_ID`] its spans carry, when they carry one.
    pub fork_id: Option<String>,
    /// The [`FORK_STEP`] its spans carry, when they carry a number.
    pub fork_step: Option<u64>,
    /// Earliest start among its tagged spans.
    pub first_start_ns: u64,
    /// Latest end among its tagged spans.
    pub last_end_ns: u64,
    /// Its spans that carry [`FORK_PARENT`].
    pub tagged_spans: usize,
    /// Whether it was diffed against its parent. When not, `skipped` says
    /// why and every field below is `null`.
    pub diffed: bool,
    /// Why it was not diffed: `deadline` (the request's query budget ran out
    /// before it), or `parent_missing` (its parent has no spans in its
    /// tenant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<&'static str>,
    /// Steps on its tape.
    pub steps: Option<usize>,
    /// How it ended ([`crate::attribution::session_outcome`]).
    pub outcome: Option<Outcome>,
    /// Its cost in USD, metered or priced.
    pub cost_usd: Option<f64>,
    /// Its spans with status `error`.
    pub errors: Option<usize>,
    /// Where it first parted from its parent, `a` being the parent and `b`
    /// the fork; `null` when it matches its parent step for step.
    pub first_divergence: Option<FirstDivergence>,
    /// The fork minus its parent.
    pub delta: Option<Delta>,
    /// Whether it parted from its parent BEFORE the step it was cut at — the
    /// replayed beginning did not re-execute as recorded (a tool that ran
    /// live and answered differently, typically). `null` when unknown.
    pub diverged_before_cut: Option<bool>,
    /// Whether either side was cut to the differ's bounds, so the alignment
    /// describes prefixes.
    pub truncated: bool,
}

/// `GET /v1/forks?run=`: a run's forks.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Forks {
    /// The parent run asked about.
    pub run: String,
    /// The parent run, summarised as read under the request's scope; `null`
    /// when it has no spans there (its forks are still listed).
    pub parent: Option<RunSide>,
    /// Its forks, the most recently started first.
    pub forks: Vec<ForkSummary>,
    /// Spans carrying [`FORK_PARENT`] whose own run id IS the parent's — a
    /// harness that forked without giving the fork a session of its own, so
    /// the fork's spans joined the parent run instead of forming a new one.
    pub merged_into_parent: usize,
    /// Tagged spans examined.
    pub spans_examined: usize,
    /// Whether the listing is incomplete: more than [`MAX_FORK_SPANS`]
    /// tagged spans, more forks than the limit, or forks the query budget
    /// did not reach (`skipped: "deadline"`).
    pub truncated: bool,
}

/// One forked run's tagged spans, folded.
struct ForkGroup {
    tenant: String,
    run: String,
    fork_id: Option<String>,
    fork_step: Option<u64>,
    first_start_ns: u64,
    last_end_ns: u64,
    tagged_spans: usize,
}

impl ForkGroup {
    fn new(tenant: String, run: String) -> Self {
        Self {
            tenant,
            run,
            fork_id: None,
            fork_step: None,
            first_start_ns: u64::MAX,
            last_end_ns: 0,
            tagged_spans: 0,
        }
    }

    fn absorb(&mut self, span: &Span) {
        self.tagged_spans += 1;
        self.first_start_ns = self.first_start_ns.min(span.start_time_ns);
        self.last_end_ns = self.last_end_ns.max(span.end_time_ns);
        if self.fork_id.is_none() {
            self.fork_id = semconv::attr_str(&span.attributes, FORK_ID)
                .filter(|id| !id.is_empty() && id.len() <= 128);
        }
        if self.fork_step.is_none() {
            self.fork_step = span
                .attributes
                .get(FORK_STEP)
                .and_then(|value| match value {
                    Value::Number(number) => number.as_u64(),
                    Value::String(text) => text.trim().parse().ok(),
                    _ => None,
                });
        }
    }

    fn summary(&self) -> ForkSummary {
        ForkSummary {
            run: self.run.clone(),
            tenant: self.tenant.clone(),
            fork_id: self.fork_id.clone(),
            fork_step: self.fork_step,
            first_start_ns: self.first_start_ns,
            last_end_ns: self.last_end_ns,
            tagged_spans: self.tagged_spans,
            diffed: false,
            skipped: None,
            steps: None,
            outcome: None,
            cost_usd: None,
            errors: None,
            first_divergence: None,
            delta: None,
            diverged_before_cut: None,
            truncated: false,
        }
    }
}

// ----------------------------------------------------------- golden runs

/// One golden run, as `GET /v1/goldens` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoldenRun {
    /// The run id.
    pub run: String,
    /// Its tenant (`""` is the default tenant).
    pub tenant: String,
    /// What marked it: `attribute` (a span recorded [`GOLDEN`] `= true`) or
    /// `annotation` (the newest annotation named [`GOLDEN`] on its session
    /// or trace says `true`).
    pub source: &'static str,
    /// When it was marked: the newest tagged span's start, or the deciding
    /// annotation's timestamp.
    pub tagged_at_ns: u64,
}

/// `GET /v1/goldens`: the runs marked as regression baselines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Goldens {
    /// The golden runs, ordered by tenant then run id, so a test report
    /// lists them the same way every time.
    pub runs: Vec<GoldenRun>,
    /// Tagged spans examined.
    pub spans_examined: usize,
    /// Annotations named [`GOLDEN`] examined.
    pub annotations_examined: usize,
    /// Whether the listing is incomplete: more than [`MAX_GOLDEN_SPANS`]
    /// tagged spans or [`MAX_GOLDEN_ANNOTATIONS`] annotations, or more runs
    /// than the limit.
    pub truncated: bool,
}

/// Whether a golden marker's value says `true`, `false`, or nothing: a
/// boolean, or the strings `true` / `false` in any case.
fn golden_flag(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) if text.eq_ignore_ascii_case("true") => Some(true),
        Value::String(text) if text.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

impl Store {
    /// The forks of `run` under `tenant` (the scoping rules of
    /// [`Store::get_run_in`]): the runs whose spans carry [`FORK_PARENT`]
    /// `= run`, the most recently started first, at most `limit` (clamped to
    /// [`MAX_FORK_LIMIT`]), each diffed against its parent read in its own
    /// tenant. `now_ns` dates the outcome rule.
    ///
    /// The tagged-span read, the parent's read and every fork's read and
    /// diff spend ONE query budget. A fork the budget does not reach is
    /// listed undiffed with `skipped: "deadline"` and the listing says
    /// `truncated` — a listing is more use partial than refused; the tagged
    /// read and the parent's read themselves are refused past the budget.
    /// `None` when neither the parent nor any fork has a span in scope.
    pub fn forks(
        &self,
        tenant: Option<&str>,
        run: &str,
        limit: usize,
        now_ns: u64,
    ) -> Result<Option<Forks>> {
        self.forks_capped(tenant, run, limit, now_ns, MAX_FORK_SPANS)
    }

    /// [`Self::forks`] with an explicit tagged-span cap, so tests can reach
    /// the truncation edge cheaply.
    pub(crate) fn forks_capped(
        &self,
        tenant: Option<&str>,
        run: &str,
        limit: usize,
        now_ns: u64,
        max_spans: usize,
    ) -> Result<Option<Forks>> {
        if run.is_empty() {
            return Err(Error::Invalid("run is required".to_owned()));
        }
        let limit = limit.clamp(1, MAX_FORK_LIMIT);
        let deadline = self.query_deadline(Instant::now());
        let filter = SpanFilter {
            attributes: vec![(FORK_PARENT.to_owned(), json!(run))],
            tenant: tenant.map(str::to_owned),
            limit: Some(max_spans.saturating_add(1)),
            ..SpanFilter::default()
        };
        let mut tagged = self.query_under(&filter, deadline)?;
        let mut truncated = tagged.len() > max_spans;
        tagged.truncate(max_spans);

        let mut merged_into_parent = 0;
        let mut groups: HashMap<(String, String), ForkGroup> = HashMap::new();
        for span in &tagged {
            let fork_run = semconv::run_id(&span.attributes, &span.trace_id);
            if fork_run == run {
                merged_into_parent += 1;
                continue;
            }
            groups
                .entry((span.tenant.clone(), fork_run.clone()))
                .or_insert_with(|| ForkGroup::new(span.tenant.clone(), fork_run))
                .absorb(span);
        }
        let mut groups: Vec<ForkGroup> = groups.into_values().collect();
        groups.sort_by(|left, right| {
            right
                .first_start_ns
                .cmp(&left.first_start_ns)
                .then_with(|| left.tenant.cmp(&right.tenant))
                .then_with(|| left.run.cmp(&right.run))
        });
        if groups.len() > limit {
            truncated = true;
            groups.truncate(limit);
        }

        let scoped = self.run_tape(tenant, run, MAX_DIFF_SPANS, MAX_DIFF_STEPS, deadline)?;
        if scoped.spans.is_empty() && groups.is_empty() && merged_into_parent == 0 {
            return Ok(None);
        }
        let parent = (!scoped.spans.is_empty()).then(|| self.side(run, &scoped, now_ns));
        // Each fork is compared with its parent IN ITS OWN TENANT, even in
        // the operator view, where the scoped read is a union across tenants.
        let mut parents: HashMap<String, RunTape> = HashMap::new();
        if let Some(own) = tenant {
            parents.insert(own.to_owned(), scoped);
        }
        let (forks, out_of_budget) =
            self.summarise_forks(run, &groups, parents, now_ns, deadline)?;
        Ok(Some(Forks {
            run: run.to_owned(),
            parent,
            forks,
            merged_into_parent,
            spans_examined: tagged.len(),
            truncated: truncated || out_of_budget,
        }))
    }

    /// Each fork summarised and diffed against its parent, in order, until
    /// the budget runs out — then the rest are listed undiffed. Also says
    /// whether it ran out.
    fn summarise_forks(
        &self,
        run: &str,
        groups: &[ForkGroup],
        mut parents: HashMap<String, RunTape>,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<(Vec<ForkSummary>, bool)> {
        let mut forks = Vec::with_capacity(groups.len());
        let mut out_of_budget = false;
        for group in groups {
            let mut summary = group.summary();
            if out_of_budget {
                summary.skipped = Some("deadline");
                forks.push(summary);
                continue;
            }
            match self.diff_fork(&mut parents, run, group, now_ns, deadline) {
                Ok(Some(diff)) => {
                    summary.diffed = true;
                    summary.steps = Some(diff.b.steps);
                    summary.outcome = Some(diff.b.outcome);
                    summary.cost_usd = Some(diff.b.cost_usd);
                    summary.errors = Some(diff.b.errors);
                    summary.diverged_before_cut = match (&diff.first_divergence, group.fork_step) {
                        (Some(first), Some(cut)) => Some((first.b as u64) < cut),
                        (None, Some(_)) => Some(false),
                        (_, None) => None,
                    };
                    summary.first_divergence = diff.first_divergence;
                    summary.delta = Some(diff.delta);
                    summary.truncated = diff.truncated;
                }
                Ok(None) => summary.skipped = Some("parent_missing"),
                Err(Error::DeadlineExceeded(_)) => {
                    out_of_budget = true;
                    summary.skipped = Some("deadline");
                }
                Err(error) => return Err(error),
            }
            forks.push(summary);
        }
        Ok((forks, out_of_budget))
    }

    /// One fork diffed against its parent, the parent read once per tenant;
    /// `None` when the parent has no spans in the fork's tenant.
    fn diff_fork(
        &self,
        parents: &mut HashMap<String, RunTape>,
        parent_run: &str,
        group: &ForkGroup,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<Option<crate::diff::RunDiff>> {
        if !parents.contains_key(&group.tenant) {
            let read = self.run_tape(
                Some(&group.tenant),
                parent_run,
                MAX_DIFF_SPANS,
                MAX_DIFF_STEPS,
                deadline,
            )?;
            parents.insert(group.tenant.clone(), read);
        }
        let parent = &parents[&group.tenant];
        if parent.spans.is_empty() {
            return Ok(None);
        }
        let fork = self.run_tape(
            Some(&group.tenant),
            &group.run,
            MAX_DIFF_SPANS,
            MAX_DIFF_STEPS,
            deadline,
        )?;
        if fork.spans.is_empty() {
            // Erased between the tagged read and this one.
            return Ok(None);
        }
        self.diff_tapes(parent_run, parent, &group.run, &fork, now_ns, deadline)
            .map(Some)
    }

    /// The golden runs under `tenant` (`None` is the operator view across
    /// every tenant), ordered by tenant then run, at most `limit` (clamped to
    /// [`MAX_GOLDEN_LIMIT`]).
    ///
    /// A run is golden when a span of it recorded [`GOLDEN`] as `true` (the
    /// boolean or the string), or when the newest annotation named
    /// [`GOLDEN`] on its session (`session_id`) or its trace (`trace_id`
    /// with no span, which names the run `trace:<trace_id>`) says `true`. A
    /// newer annotation saying `false` un-marks a run, whatever its spans
    /// say: the annotation is the later decision. Span-subject annotations
    /// and values that are neither true nor false decide nothing.
    pub fn goldens(&self, tenant: Option<&str>, limit: usize) -> Result<Goldens> {
        let limit = limit.clamp(1, MAX_GOLDEN_LIMIT);
        let deadline = self.query_deadline(Instant::now());
        let filter = SpanFilter {
            attributes: vec![(GOLDEN.to_owned(), json!(true))],
            tenant: tenant.map(str::to_owned),
            limit: Some(MAX_GOLDEN_SPANS + 1),
            ..SpanFilter::default()
        };
        let mut tagged = self.query_under(&filter, deadline)?;
        let mut truncated = tagged.len() > MAX_GOLDEN_SPANS;
        tagged.truncate(MAX_GOLDEN_SPANS);

        let mut runs: BTreeMap<(String, String), GoldenRun> = BTreeMap::new();
        for span in &tagged {
            let run = semconv::run_id(&span.attributes, &span.trace_id);
            let entry = runs
                .entry((span.tenant.clone(), run.clone()))
                .or_insert_with(|| GoldenRun {
                    run,
                    tenant: span.tenant.clone(),
                    source: "attribute",
                    tagged_at_ns: 0,
                });
            entry.tagged_at_ns = entry.tagged_at_ns.max(span.start_time_ns);
        }

        let mut annotations = self.search_annotations(&AnnotationQuery {
            trace_id: None,
            span_id: None,
            tenant,
            session_id: None,
            experiment_id: None,
            example_id: None,
            name: Some(GOLDEN),
            source_prefix: None,
            since_ns: None,
            until_ns: None,
            limit: Some(MAX_GOLDEN_ANNOTATIONS + 1),
        })?;
        truncated |= annotations.len() > MAX_GOLDEN_ANNOTATIONS;
        annotations.truncate(MAX_GOLDEN_ANNOTATIONS);
        Deadline::check(deadline, 0)?;
        // Newest first: the first decision seen for a run is its newest.
        let mut decided: std::collections::HashSet<(String, String)> = Default::default();
        for annotation in &annotations {
            let run = if !annotation.session_id.is_empty() {
                annotation.session_id.clone()
            } else if !annotation.trace_id.is_empty()
                && annotation.span_id.is_empty()
                && annotation.experiment_id.is_none()
            {
                format!("{}{}", semconv::TRACE_RUN_PREFIX, annotation.trace_id)
            } else {
                continue;
            };
            let Some(flag) = golden_flag(&annotation.value) else {
                continue;
            };
            let key = (annotation.tenant.clone(), run);
            if !decided.insert(key.clone()) {
                continue;
            }
            if flag {
                runs.insert(
                    key.clone(),
                    GoldenRun {
                        run: key.1,
                        tenant: key.0,
                        source: "annotation",
                        tagged_at_ns: annotation.timestamp_ns,
                    },
                );
            } else {
                runs.remove(&key);
            }
        }
        let mut runs: Vec<GoldenRun> = runs.into_values().collect();
        if runs.len() > limit {
            truncated = true;
            runs.truncate(limit);
        }
        Ok(Goldens {
            runs,
            spans_examined: tagged.len(),
            annotations_examined: annotations.len(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::{span, TempStore};

    #[test]
    fn a_fork_spec_reads_edits_and_a_live_point() {
        let spec = ForkSpec::parse(
            Some(&json!([
                {"step": 4, "tool_result": null},
                {"step": 2, "response": "It is sunny."},
            ])),
            Some(&json!(4)),
        )
        .expect("parses");
        assert_eq!(spec.live_after, Some(4));
        assert_eq!(
            spec.edits.iter().map(|edit| edit.step).collect::<Vec<_>>(),
            [2, 4],
            "edits come back in step order"
        );
        assert_eq!(spec.edits[1].value, EditValue::ToolResult(Value::Null));
        assert!(spec.is_fork());
        assert_eq!(spec.first_edit(), Some(2));
        assert_eq!(spec.cut(), Some(2));

        // A live point alone cuts at the step after it.
        let rewind = ForkSpec::parse(None, Some(&json!(6))).expect("parses");
        assert_eq!((rewind.first_edit(), rewind.cut()), (None, Some(7)));
        assert!(!ForkSpec::parse(None, None).expect("parses").is_fork());
        assert!(!ForkSpec::default().is_fork());
    }

    #[test]
    fn a_malformed_fork_spec_is_refused_naming_the_problem() {
        let refused = |edits: Value, live: Value| {
            ForkSpec::parse(Some(&edits), Some(&live)).expect_err("refused")
        };
        assert!(refused(json!({"step": 1}), Value::Null).contains("must be a list"));
        assert!(refused(json!([{"step": 1}]), Value::Null).contains("edit 0 must be"));
        assert!(refused(json!([{"step": -1, "response": "x"}]), Value::Null)
            .contains("non-negative integer"));
        assert!(refused(json!([{"step": 1, "response": null}]), Value::Null).contains("is null"));
        assert!(refused(
            json!([{"step": 1, "response": "x", "tool_result": 1}]),
            Value::Null
        )
        .contains("both"));
        assert!(refused(json!([{"step": 1, "answer": "x"}]), Value::Null)
            .contains("unknown field: answer"));
        assert!(refused(
            json!([{"step": 3, "response": "x"}, {"step": 3, "response": "y"}]),
            Value::Null
        )
        .contains("two edits name step 3"));
        assert!(refused(json!([{"step": 5, "response": "x"}]), json!(4))
            .contains("comes after live_after 4"));
        assert!(refused(json!([]), json!("7")).contains("live_after must be"));
        let many: Vec<Value> = (0..=MAX_EDITS)
            .map(|step| json!({"step": step, "response": "x"}))
            .collect();
        assert!(refused(Value::Array(many), Value::Null).contains("at most 64 edits"));
    }

    fn weather_run(store: &Store, session: &str, trace: &str, extra: Value) {
        let with = |mut attributes: Value| {
            attributes["session.id"] = json!(session);
            if let (Some(target), Some(more)) = (attributes.as_object_mut(), extra.as_object()) {
                for (key, value) in more {
                    target.insert(key.clone(), value.clone());
                }
            }
            attributes
        };
        store
            .ingest_batch(vec![
                span(
                    trace,
                    "a-llm",
                    100,
                    with(json!({
                        "gen_ai.operation.name": "chat",
                        "gen_ai.request.model": "gpt-4o",
                        "gen_ai.prompt.0.role": "user",
                        "gen_ai.prompt.0.content": "Weather in Paris?",
                        "gen_ai.completion.0.role": "assistant",
                        "gen_ai.completion.0.tool_calls.0.id": "c1",
                        "gen_ai.completion.0.tool_calls.0.name": "get_weather",
                        "gen_ai.completion.0.tool_calls.0.arguments": "{\"city\":\"Paris\"}",
                    })),
                ),
                span(
                    trace,
                    "b-tool",
                    200,
                    with(json!({
                        "gen_ai.operation.name": "execute_tool",
                        "gen_ai.tool.name": "get_weather",
                        "gen_ai.tool.call.arguments": "{\"city\":\"Paris\"}",
                        "gen_ai.tool.call.result": "rainy",
                    })),
                ),
            ])
            .expect("ingests");
    }

    #[test]
    fn a_fork_spec_is_checked_against_the_run() {
        let fixture = TempStore::open("fork-check", crate::Config::default());
        let store = fixture.store();
        weather_run(store, "run-1", "t1", json!({}));
        let tape = store
            .tape(Some(""), "run-1", crate::tape::TapeOptions::default())
            .expect("reads")
            .expect("exists");
        let check = |edits: Value, live: Value| {
            ForkSpec::parse(Some(&edits), Some(&live))
                .expect("parses")
                .check_against(&tape.steps)
        };
        assert!(check(json!([{"step": 0, "response": "hi"}]), json!(1)).is_ok());
        assert!(check(json!([{"step": 1, "tool_result": "sunny"}]), Value::Null).is_ok());
        let wrong_kind = check(json!([{"step": 1, "response": "hi"}]), Value::Null).unwrap_err();
        assert!(
            wrong_kind.contains("step 1 is a tool call (tool:get_weather)")
                && wrong_kind.contains("\"tool_result\""),
            "{wrong_kind}"
        );
        let past = check(json!([{"step": 2, "tool_result": 1}]), Value::Null).unwrap_err();
        assert!(past.contains("past the run's last step (1)"), "{past}");
        let live = check(json!([]), json!(2)).unwrap_err();
        assert!(live.contains("live_after 2 is past"), "{live}");
    }

    #[test]
    fn forks_are_found_by_their_tags_and_diffed_against_the_parent() {
        let fixture = TempStore::open("fork-listing", crate::Config::default());
        let store = fixture.store();
        weather_run(store, "parent", "t1", json!({}));
        // A fork that re-executed step 0 and got an edited tool result.
        store
            .ingest_batch(vec![
                span(
                    "t2",
                    "a-llm",
                    1_000,
                    json!({
                        "session.id": "fork-a", FORK_PARENT: "parent", FORK_STEP: 1,
                        FORK_ID: "fk_aaa",
                        "gen_ai.operation.name": "chat",
                        "gen_ai.request.model": "gpt-4o",
                        "gen_ai.prompt.0.role": "user",
                        "gen_ai.prompt.0.content": "Weather in Paris?",
                        "gen_ai.completion.0.role": "assistant",
                        "gen_ai.completion.0.tool_calls.0.id": "c1",
                        "gen_ai.completion.0.tool_calls.0.name": "get_weather",
                        "gen_ai.completion.0.tool_calls.0.arguments": "{\"city\":\"Paris\"}",
                    }),
                ),
                span(
                    "t2",
                    "b-tool",
                    1_100,
                    json!({
                        "session.id": "fork-a", FORK_PARENT: "parent", FORK_STEP: "1",
                        "gen_ai.operation.name": "execute_tool",
                        "gen_ai.tool.name": "get_weather",
                        "gen_ai.tool.call.arguments": "{\"city\":\"Paris\"}",
                        "gen_ai.tool.call.result": "sunny",
                    }),
                ),
                // A harness that forgot the fork's own session: its span
                // joined the parent run.
                span(
                    "t1",
                    "c-stray",
                    300,
                    json!({"session.id": "parent", FORK_PARENT: "parent"}),
                ),
            ])
            .expect("ingests");
        let forks = store
            .forks(Some(""), "parent", 10, u64::MAX)
            .expect("lists")
            .expect("exists");
        assert_eq!(forks.forks.len(), 1);
        assert_eq!(forks.merged_into_parent, 1);
        let fork = &forks.forks[0];
        assert_eq!(fork.run, "fork-a");
        assert_eq!(fork.fork_id.as_deref(), Some("fk_aaa"));
        assert_eq!(fork.fork_step, Some(1));
        assert_eq!(fork.tagged_spans, 2);
        assert!(fork.diffed);
        let first = fork.first_divergence.as_ref().expect("diverges");
        assert_eq!(
            (first.a, first.b),
            (1, 1),
            "the tool result is where they part"
        );
        assert_eq!(fork.diverged_before_cut, Some(false));
        assert!(!forks.truncated);

        // Another tenant sees none of it.
        assert!(store
            .forks(Some("acme"), "parent", 10, u64::MAX)
            .expect("lists")
            .is_none());
        // Past the tagged-span cap the listing says so.
        let capped = store
            .forks_capped(Some(""), "parent", 10, u64::MAX, 1)
            .expect("lists")
            .expect("exists");
        assert!(capped.truncated);
        assert_eq!(capped.spans_examined, 1);
    }

    #[test]
    fn forks_the_budget_does_not_reach_are_listed_undiffed_not_refused() {
        let fixture = TempStore::open("fork-deadline", crate::Config::default());
        let store = fixture.store();
        weather_run(store, "parent", "t1", json!({}));
        weather_run(store, "fork-a", "t2", json!({FORK_PARENT: "parent"}));
        let mut group = ForkGroup::new(String::new(), "fork-a".to_owned());
        group.tagged_spans = 2;
        let groups = [group];
        // A budget that ran out before the first comparison began.
        let spent = Deadline::starting(
            Instant::now() - std::time::Duration::from_secs(1),
            Some(std::time::Duration::from_millis(1)),
        );
        let (forks, out_of_budget) = store
            .summarise_forks("parent", &groups, HashMap::new(), u64::MAX, spent)
            .expect("a listing, not a refusal");
        assert!(out_of_budget);
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0].skipped, Some("deadline"));
        assert!(!forks[0].diffed && forks[0].first_divergence.is_none());
        // With budget left, the same fork is diffed.
        let (forks, out_of_budget) = store
            .summarise_forks("parent", &groups, HashMap::new(), u64::MAX, None)
            .expect("lists");
        assert!(!out_of_budget && forks[0].diffed);
    }

    #[test]
    fn goldens_come_from_attributes_and_the_newest_annotation_decides() {
        let fixture = TempStore::open("goldens", crate::Config::default());
        let store = fixture.store();
        weather_run(store, "by-attr", "t1", json!({GOLDEN: true}));
        weather_run(store, "by-string", "t2", json!({GOLDEN: "true"}));
        weather_run(store, "un-marked", "t3", json!({GOLDEN: true}));
        weather_run(store, "by-note", "t4", json!({}));
        let note = |session: &str, value: Value, at: u64| crate::annotations::Annotation {
            trace_id: String::new(),
            span_id: String::new(),
            tenant: String::new(),
            session_id: session.to_owned(),
            experiment_id: None,
            example_id: String::new(),
            name: GOLDEN.to_owned(),
            value,
            source: "human:test".to_owned(),
            comment: String::new(),
            timestamp_ns: at,
        };
        store
            .annotate(note("by-note", json!(false), 10))
            .expect("annotates");
        store
            .annotate(note("by-note", json!(true), 20))
            .expect("annotates");
        store
            .annotate(note("un-marked", json!("FALSE"), 30))
            .expect("annotates");
        store
            .annotate(note("by-attr", json!(7), 40))
            .expect("annotates");
        let goldens = store.goldens(Some(""), 100).expect("lists");
        let listed: Vec<(&str, &str)> = goldens
            .runs
            .iter()
            .map(|run| (run.run.as_str(), run.source))
            .collect();
        assert_eq!(
            listed,
            [
                ("by-attr", "attribute"),
                ("by-note", "annotation"),
                ("by-string", "attribute")
            ]
        );
        assert_eq!(goldens.runs[1].tagged_at_ns, 20);
        assert!(!goldens.truncated);
        let one = store.goldens(Some(""), 1).expect("lists");
        assert!(one.truncated && one.runs.len() == 1);
        assert!(store
            .goldens(Some("acme"), 100)
            .expect("lists")
            .runs
            .is_empty());
    }
}
