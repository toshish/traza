//! Change Radar: what changed in an agent, when, and what it did.
//!
//! "It worked last week" is the most common sentence in agent operations, and
//! the hardest to act on: a system prompt was edited, a tool was added, the
//! provider rolled a new snapshot behind the same model alias, a deploy
//! bumped three things at once — and nobody can say which of them, if any,
//! is why the error rate doubled. Traza holds every run, so it can answer
//! from evidence. This module reads a window of an agent's runs and returns
//! a ranked timeline of **change points**, each with what changed, when, how
//! many runs ran on either side, what moved (error rate, success rate, cost,
//! steps, duration, tool mix — with p-values), and a verdict.
//!
//! # What is compared
//!
//! A run's **configuration fingerprint** is computed from its spans on every
//! read (nothing is stored): the system prompt, the tool catalog, the
//! requested and returned models, the sampling parameters, the service
//! version and the telemetry SDK version (the `fingerprint` submodule names
//! every key).
//! A run carrying several values of one dimension — a planner model and an
//! executor model — has the SET of them as its value.
//!
//! Runs are grouped into **cohorts** by `(tenant, agent)`, where the agent is
//! the first step's agent name ([`semconv::step_facts`]) or else the run's
//! earliest span's service — the Fate Map's cohort rule, so the two features
//! talk about the same agents.
//!
//! # Change points
//!
//! Per dimension, the cohort's runs are walked in start order through a
//! sliding window of `clamp(n/8, 8, 50)` runs. A value becomes **dominant**
//! when it holds at least half the window and leads the incumbent by
//! `max(2, window/10)` runs; the change point is placed at the first run of
//! the split that best separates incumbent from newcomer inside that window
//! — for a clean deploy, exactly the first run on the new value. A value
//! that never becomes dominant (a 5% canary, an A/B arm, a one-off) is not a
//! change point; a newcomer that loses dominance again within one window is
//! an excursion and its change point is withdrawn. A fresh value that has
//! held every run since it appeared, at the very end of the sequence, is a
//! change point from its third run on — a deploy too recent to fill half a
//! window, and usually `insufficient_data`. A dimension where more
//! than half the runs carry a value no other run shares is **volatile** —
//! typically a prompt embedding something per-request that masking did not
//! catch — and is reported as such instead of being searched.
//!
//! **Behaviour** changes are the ones no configuration explains: the
//! distribution of the cohort's top-level paths (each run's first
//! [`BEHAVIOR_PATH_DEPTH`] step tokens, consecutive repeats bucketed `x`,
//! `x×2`, `x×3-5`, `x×6+` as the Fate Map buckets them) is compared between
//! adjacent windows by Jensen–Shannon distance. A peak at or above the
//! threshold (default [`DEFAULT_BEHAVIOR_THRESHOLD`]) that a deterministic
//! permutation test calls significant (p < [`BEHAVIOR_ALPHA`]), with no
//! configuration change within a window of it, is reported as dimension
//! `behavior` — the provider changed under you. Every configuration change
//! also carries the same distance between its two sides (`behavior_shift`),
//! so "the prompt changed AND the agent now takes different paths" is
//! visible.
//!
//! # Deltas and verdicts
//!
//! Each change compares the runs on its old value just before it with the
//! runs on its new value just after it, each side bounded by the
//! neighbouring change points (co-changes within [`CO_CHANGE_RUNS`] runs
//! excepted) and by [`MAX_SIDE_RUNS`]. Runs still active (last span within
//! the session idle window) are left out: their cost and steps are partial.
//! Rates (error rate: runs with at least one error span; success rate: runs
//! that ended success, of those that ended at all) use the pooled
//! two-proportion z-test; cost, steps and duration use Welch's t-test on the
//! logarithm (cost over priced runs only; steps and duration as `ln(1+x)`),
//! so one runaway run does not decide a verdict. Holm's correction holds the
//! family-wise error rate across a change's five tests at 5%. The verdict is
//! `regression` when a quality metric (error or success rate) got
//! significantly worse — or, with quality unchanged, an efficiency metric
//! did — `improvement` symmetrically, `neutral` when nothing is significant,
//! and `insufficient_data` with fewer than [`MIN_RUNS_PER_SIDE`] runs on a
//! side. Changes are ranked regressions first, by effect size (Cohen's h for
//! rates, d for log-scale means).
//!
//! # What this is not
//!
//! Correlation in time, not causation: a change point says the configuration
//! changed and the outcomes moved at the same moment. Co-changes are listed
//! precisely because the radar cannot tell which of several simultaneous
//! changes did it. The statistics assume runs are independent; a burst of
//! retries of one task is not.
//!
//! # Bounds
//!
//! One read of the window under the query deadline. Only agent runs (at
//! least one llm, tool or agent step) are analysed. At most [`max_runs`]
//! runs (the newest agent runs; the window then starts later and says so),
//! [`MAX_RUN_STEPS`] earliest steps per run, [`MAX_RUN_VALUES`] values per
//! dimension per run, [`MAX_VALUES_PER_DIMENSION`] distinct values per
//! dimension, [`MAX_AGENTS`] cohorts and [`MAX_CHANGES_PER_AGENT`] changes
//! each. Tenant-scoped like every read: a bound caller folds its own tenant's
//! spans and nothing else.

mod fingerprint;
mod stats;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::attribution::{self, Outcome};
use crate::semconv::{self, StepKind};
use crate::{Deadline, Error, Result, Span, SpanFilter, Store};

use fingerprint::{Fingerprint, Sample};

/// The route this module serves.
pub const ROUTE: &str = "/v1/changes";

/// The window read when the caller names no `since`: thirty days.
pub const DEFAULT_WINDOW_NS: u64 = 30 * 86_400 * 1_000_000_000;

/// Runs one analysis reads when the server is not told otherwise
/// (`--radar-max-runs`).
pub const DEFAULT_MAX_RUNS: usize = 20_000;

/// Cohorts reported at most; past it, the ones with the most runs.
pub const MAX_AGENTS: usize = 100;

/// Changes reported per cohort at most, after ranking.
pub const MAX_CHANGES_PER_AGENT: usize = 40;

/// Runs a side needs for a verdict other than `insufficient_data`.
pub const MIN_RUNS_PER_SIDE: usize = 10;

/// Runs of the incumbent value a dimension needs before a newcomer's
/// dominance is a change rather than the dimension settling.
pub const MIN_ESTABLISHED_RUNS: usize = 3;

/// Change points of different dimensions this many runs apart or closer are
/// one event: each lists the others as `co_changes`.
pub const CO_CHANGE_RUNS: usize = 2;

/// Runs compared on each side of a change at most — the ones nearest it.
pub const MAX_SIDE_RUNS: usize = 2_000;

/// Step tokens in a top-level path.
pub const BEHAVIOR_PATH_DEPTH: usize = 4;

/// The Jensen–Shannon distance at or above which a path-distribution shift
/// is a behaviour change candidate (`behavior_threshold=` overrides). About
/// a sixth of runs moving to a new path.
pub const DEFAULT_BEHAVIOR_THRESHOLD: f64 = 0.3;

/// The permutation-test p-value a behaviour change candidate must beat.
pub const BEHAVIOR_ALPHA: f64 = 0.01;

/// Relabellings in a permutation test: resolution 1/200.
pub const BEHAVIOR_PERMUTATIONS: usize = 199;

/// Family-wise error rate of one change's metric tests (Holm).
pub const VERDICT_ALPHA: f64 = 0.05;

/// Earliest steps kept per run — enough for the top-level path.
pub const MAX_RUN_STEPS: usize = 32;

/// Distinct values of one dimension kept per run; a run carrying more is
/// shown with a trailing ` + …`.
pub const MAX_RUN_VALUES: usize = 4;

/// Distinct values per dimension per analysis; further values read as
/// `(other)`.
pub const MAX_VALUES_PER_DIMENSION: usize = 2_048;

const MAX_TOKENS: usize = 8_192;
const MAX_TOOLS_PER_RUN: usize = 64;
const MAX_ENVIRONMENTS_PER_RUN: usize = 4;
const MAX_EVICTED_RUNS: usize = 1_000_000;
const TOOL_MIX_ROWS: usize = 8;
const PATH_ROWS: usize = 5;
const EXAMPLE_RUNS: usize = 3;
const DIFF_CHARS: usize = 80;
/// Runs per side a configuration change's behaviour shift is tested on.
const SHIFT_SIDE_RUNS: usize = 200;
const MIN_BEHAVIOR_WINDOW: usize = 15;
const MAX_BEHAVIOR_WINDOW: usize = 100;
/// Behaviour candidates (distance peaks over the threshold) tested per
/// cohort, strongest first.
const MAX_BEHAVIOR_CANDIDATES: usize = 16;
const MIN_DETECTION_WINDOW: usize = 8;
const MAX_DETECTION_WINDOW: usize = 50;
/// A dimension is volatile when this share of its runs carry a value no
/// other run shares (and it has at least [`VOLATILE_MIN_RUNS`] runs).
const VOLATILE_UNIQUE_SHARE: f64 = 0.5;
const VOLATILE_MIN_RUNS: usize = 20;
const OTHER: u32 = u32::MAX;

static CONFIGURED_MAX_RUNS: OnceLock<usize> = OnceLock::new();

/// Sets the process's run cap (`--radar-max-runs`). Called once by the
/// server before it serves; later calls are ignored.
pub fn configure(max_runs: usize) {
    let _ = CONFIGURED_MAX_RUNS.set(max_runs.max(1));
}

/// The run cap the HTTP route and the MCP tool apply: what [`configure`] set,
/// else [`DEFAULT_MAX_RUNS`].
pub fn max_runs() -> usize {
    CONFIGURED_MAX_RUNS
        .get()
        .copied()
        .unwrap_or(DEFAULT_MAX_RUNS)
}

/// What can change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// The system / developer instructions, volatile tokens masked.
    SystemPrompt,
    /// The tools offered to the model.
    ToolCatalog,
    /// The model asked for.
    ModelRequested,
    /// The model that answered.
    ModelReturned,
    /// Sampling parameters (`gen_ai.request.*` but the model).
    Params,
    /// `service.version`.
    ServiceVersion,
    /// `telemetry.sdk.name` / `telemetry.sdk.version`.
    SdkVersion,
    /// The agent's top-level paths, with no configuration change.
    Behavior,
}

impl Dimension {
    /// The configuration dimensions, in report order.
    pub const CONFIG: [Dimension; 7] = [
        Self::SystemPrompt,
        Self::ToolCatalog,
        Self::ModelRequested,
        Self::ModelReturned,
        Self::Params,
        Self::ServiceVersion,
        Self::SdkVersion,
    ];

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemPrompt => "system_prompt",
            Self::ToolCatalog => "tool_catalog",
            Self::ModelRequested => "model_requested",
            Self::ModelReturned => "model_returned",
            Self::Params => "params",
            Self::ServiceVersion => "service_version",
            Self::SdkVersion => "sdk_version",
            Self::Behavior => "behavior",
        }
    }

    /// A phrase for sentences.
    pub fn noun(self) -> &'static str {
        match self {
            Self::SystemPrompt => "system prompt",
            Self::ToolCatalog => "tool catalog",
            Self::ModelRequested => "requested model",
            Self::ModelReturned => "returned model",
            Self::Params => "sampling parameters",
            Self::ServiceVersion => "service version",
            Self::SdkVersion => "telemetry SDK",
            Self::Behavior => "behaviour",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::SystemPrompt => 0,
            Self::ToolCatalog => 1,
            Self::ModelRequested => 2,
            Self::ModelReturned => 3,
            Self::Params => 4,
            Self::ServiceVersion => 5,
            Self::SdkVersion => 6,
            Self::Behavior => 7,
        }
    }
}

/// What a change did, all told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Quality, or with quality unchanged efficiency, got significantly worse.
    Regression,
    /// Significantly better, by the same rule.
    Improvement,
    /// Nothing moved significantly.
    Neutral,
    /// Too few runs on a side to say.
    InsufficientData,
}

impl Verdict {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regression => "regression",
            Self::Improvement => "improvement",
            Self::Neutral => "neutral",
            Self::InsufficientData => "insufficient_data",
        }
    }
}

/// One change analysis's question.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeQuery {
    /// Only this cohort (exact agent name).
    pub agent: Option<String>,
    /// Only runs reporting this `deployment.environment(.name)`.
    pub environment: Option<String>,
    /// Window start, Unix ns (spans starting at or after).
    pub since_ns: u64,
    /// Window end, Unix ns (spans starting at or before).
    pub until_ns: u64,
    /// Jensen–Shannon distance a behaviour change must reach.
    pub behavior_threshold: f64,
    /// Runs read at most.
    pub max_runs: usize,
    /// One run to place on its agent's timeline: the analysis is narrowed to
    /// that run's cohort, and the report's [`ChangeReport::run`] says which
    /// change points the run is downstream of.
    pub run: Option<String>,
}

impl ChangeQuery {
    /// The default question at `now_ns`: the last thirty days, every agent,
    /// the default threshold, the process's run cap.
    pub fn recent(now_ns: u64) -> Self {
        Self {
            agent: None,
            environment: None,
            since_ns: now_ns.saturating_sub(DEFAULT_WINDOW_NS),
            until_ns: now_ns,
            behavior_threshold: DEFAULT_BEHAVIOR_THRESHOLD,
            max_runs: max_runs(),
            run: None,
        }
    }
}

/// A change point one run is downstream of.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunChange {
    /// What changed.
    pub dimension: Dimension,
    /// When, Unix ns.
    pub at_ns: u64,
    /// The value the run carries, introduced by this change (for
    /// `behavior`, the path era the run started in).
    pub to: String,
    /// The change's verdict.
    pub verdict: Verdict,
    /// The change's explanation.
    pub explanation: String,
}

/// One run placed on its agent's timeline.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunContext {
    /// The run id.
    pub run: String,
    /// Its tenant.
    pub tenant: String,
    /// Its agent (cohort).
    pub agent: String,
    /// Its first span's start, Unix ns.
    pub start_ns: u64,
    /// How it ended (`unknown` while still active).
    pub outcome: Outcome,
    /// Whether any of its spans failed.
    pub errored: bool,
    /// Its value of every configuration dimension it carries, and its
    /// top-level `path`.
    pub fingerprint: BTreeMap<String, String>,
    /// The latest change point of each dimension that introduced the value
    /// the run carries and started at or before it — what the run is
    /// downstream of. Empty when it ran on values that predate every change.
    pub downstream_of: Vec<RunChange>,
}

/// A rate before and after a change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RateDelta {
    /// The rate on the old side, `0..=1`; `null` with no runs to rate.
    pub before: Option<f64>,
    /// The rate on the new side.
    pub after: Option<f64>,
    /// Two-proportion z-test, two-sided; `null` when it cannot be computed.
    pub p_value: Option<f64>,
    /// Runs rated on the old side.
    pub n_before: usize,
    /// Runs rated on the new side.
    pub n_after: usize,
}

/// A per-run mean before and after a change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MeanDelta {
    /// The arithmetic mean per run on the old side.
    pub before: Option<f64>,
    /// The arithmetic mean per run on the new side.
    pub after: Option<f64>,
    /// `after / before`, when `before` is positive.
    pub ratio: Option<f64>,
    /// Welch's t-test on the log scale, two-sided.
    pub p_value: Option<f64>,
    /// Runs measured on the old side.
    pub n_before: usize,
    /// Runs measured on the new side.
    pub n_after: usize,
}

/// One tool's share of tool calls on each side.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolShare {
    /// The tool name.
    pub tool: String,
    /// Its share of the old side's tool calls.
    pub before_share: f64,
    /// Its share of the new side's tool calls.
    pub after_share: f64,
}

/// What moved across a change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Deltas {
    /// Share of runs with at least one error span.
    pub error_rate: RateDelta,
    /// Share of ended runs that ended in success.
    pub success_rate: RateDelta,
    /// Cost in USD per run (metered or priced).
    pub cost_per_run: MeanDelta,
    /// Steps (llm, tool and agent spans) per run.
    pub steps_per_run: MeanDelta,
    /// First start to last end per run, in nanoseconds.
    pub duration_ns_per_run: MeanDelta,
    /// The tools whose share is largest on either side.
    pub tool_mix: Vec<ToolShare>,
}

/// A path-distribution shift.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Shift {
    /// Jensen–Shannon distance (base 2) between the two sides' top-level
    /// path distributions, `0..=1`.
    pub js_distance: f64,
    /// Deterministic permutation-test p-value.
    pub p_value: f64,
}

/// The runs nearest a change on each side.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ExampleRuns {
    /// The last runs on the old side.
    pub before: Vec<String>,
    /// The first runs on the new side.
    pub after: Vec<String>,
}

/// One change point.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    /// Start of the first run on the new value, Unix ns.
    pub at_ns: u64,
    /// What changed.
    pub dimension: Dimension,
    /// The old value (a digest for prompts and catalogs, else the value; a
    /// run set is joined with ` + `). For `behavior`, the old side's most
    /// common top-level path.
    pub from: String,
    /// The new value, likewise.
    pub to: String,
    /// Runs compared on the old side.
    pub runs_before: usize,
    /// Runs compared on the new side.
    pub runs_after: usize,
    /// What moved.
    pub deltas: Deltas,
    /// What that adds up to.
    pub verdict: Verdict,
    /// The effect size that decided the verdict (absolute; 0 when neutral).
    pub effect_size: f64,
    /// One sentence a reader can check against the deltas.
    pub explanation: String,
    /// Other dimensions that changed within [`CO_CHANGE_RUNS`] runs.
    pub co_changes: Vec<Dimension>,
    /// The two sides' path-distribution distance, when both sides have
    /// enough runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior_shift: Option<Shift>,
    /// Dimension-specific detail: a system prompt's text diff, a catalog's
    /// added and removed tools, the parameters that differ, the paths that
    /// moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    /// Runs to open on each side.
    pub example_runs: ExampleRuns,
}

/// One cohort's timeline.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentChanges {
    /// The agent.
    pub agent: String,
    /// The cohort's tenant (`""` is the default tenant).
    pub tenant: String,
    /// Runs in the cohort, in the window.
    pub runs: usize,
    /// Earliest run start, Unix ns.
    pub first_run_ns: u64,
    /// Latest run start, Unix ns.
    pub last_run_ns: u64,
    /// One sentence: the headline.
    pub summary: String,
    /// The value each configuration dimension holds now (dominant over the
    /// most recent runs).
    pub current: BTreeMap<String, String>,
    /// Dimensions too varied to search (see the module docs).
    pub volatile: Vec<Dimension>,
    /// Change points, ranked.
    pub changes: Vec<Change>,
    /// Whether changes past [`MAX_CHANGES_PER_AGENT`] were cut.
    pub truncated: bool,
}

/// A change analysis.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChangeReport {
    /// Window start asked for, Unix ns.
    pub since_ns: u64,
    /// Window end, Unix ns.
    pub until_ns: u64,
    /// When the run cap applied: the start of the runs actually analysed
    /// (every run starting after it is included in full; earlier ones are
    /// not).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_since_ns: Option<u64>,
    /// Spans read.
    pub spans_scanned: u64,
    /// Runs analysed, across every cohort reported.
    pub runs_scanned: usize,
    /// Cohorts, those with a regression first.
    pub agents: Vec<AgentChanges>,
    /// When the question named a run and it was analysed: where it sits on
    /// its agent's timeline. Absent when no run was named, or when it was
    /// not found among the window's analysed runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunContext>,
    /// Whether any bound cut the answer: the run cap, the cohort cap.
    pub truncated: bool,
}

// --------------------------------------------------------------- accumulate

/// A string interner with a cap: past it every new string is [`OTHER`].
struct Interner {
    ids: HashMap<String, u32>,
    names: Vec<String>,
    cap: usize,
}

impl Interner {
    fn new(cap: usize) -> Self {
        Self {
            ids: HashMap::new(),
            names: Vec::new(),
            cap,
        }
    }

    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.ids.get(text) {
            return id;
        }
        if self.names.len() >= self.cap {
            return OTHER;
        }
        let id = self.names.len() as u32;
        self.names.push(text.to_owned());
        self.ids.insert(text.to_owned(), id);
        id
    }

    fn name(&self, id: u32) -> &str {
        self.names
            .get(id as usize)
            .map_or("(other)", String::as_str)
    }
}

/// One dimension's distinct values: identity, display and sample.
struct ValueTable {
    keys: Interner,
    displays: Vec<String>,
    samples: Vec<Option<Sample>>,
}

impl ValueTable {
    fn new() -> Self {
        Self {
            keys: Interner::new(MAX_VALUES_PER_DIMENSION),
            displays: Vec::new(),
            samples: Vec::new(),
        }
    }

    fn intern(&mut self, fingerprint: Fingerprint) -> u32 {
        let id = self.keys.intern(&fingerprint.key);
        if id as usize == self.displays.len() {
            self.displays.push(fingerprint.display);
            self.samples.push(fingerprint.sample);
        }
        id
    }

    fn display(&self, id: u32) -> &str {
        self.displays
            .get(id as usize)
            .map_or("(other)", String::as_str)
    }

    fn sample(&self, id: u32) -> Option<&Sample> {
        self.samples.get(id as usize).and_then(Option::as_ref)
    }
}

struct Tables {
    tokens: Interner,
    tools: Interner,
    environments: Interner,
    values: Vec<ValueTable>,
}

impl Tables {
    fn new() -> Self {
        Self {
            tokens: Interner::new(MAX_TOKENS),
            tools: Interner::new(MAX_TOKENS),
            environments: Interner::new(256),
            values: Dimension::CONFIG
                .iter()
                .map(|_| ValueTable::new())
                .collect(),
        }
    }
}

/// A run's distinct values of one dimension.
#[derive(Clone, Debug, Default)]
struct ValueSet {
    ids: Vec<u32>,
    overflow: bool,
}

impl ValueSet {
    fn add(&mut self, id: u32) {
        if self.ids.contains(&id) {
            return;
        }
        if self.ids.len() >= MAX_RUN_VALUES {
            self.overflow = true;
            return;
        }
        self.ids.push(id);
    }
}

type SpanKey = (u64, String, String);

fn key_less(time: u64, span: &Span, than: &Option<SpanKey>) -> bool {
    match than {
        None => true,
        Some((t, trace, id)) => {
            (time, span.trace_id.as_str(), span.span_id.as_str())
                < (*t, trace.as_str(), id.as_str())
        }
    }
}

fn key_of(time: u64, span: &Span) -> SpanKey {
    (time, span.trace_id.clone(), span.span_id.clone())
}

/// Everything one run contributes, folded span by span in bounded memory.
#[derive(Default)]
struct RunAcc {
    earliest: Option<SpanKey>,
    earliest_service: String,
    first_step: Option<SpanKey>,
    first_step_agent: Option<String>,
    last: Option<SpanKey>,
    last_error: bool,
    last_outcome: Option<String>,
    declared: Option<(SpanKey, String)>,
    errors: u32,
    cost: f64,
    steps: u32,
    step_keys: Vec<(u64, u64, u32)>,
    tools: Vec<(u32, u32)>,
    values: [ValueSet; 7],
    environments: Vec<u32>,
}

impl RunAcc {
    fn first_start(&self) -> u64 {
        self.earliest.as_ref().map_or(u64::MAX, |key| key.0)
    }

    fn absorb(&mut self, span: &Span, llm: &semconv::LlmFacts, tables: &mut Tables) {
        let error = span.status.eq_ignore_ascii_case("error");
        if error {
            self.errors += 1;
        }
        self.cost += llm.cost_usd.filter(|cost| cost.is_finite()).unwrap_or(0.0);
        if key_less(span.start_time_ns, span, &self.earliest) {
            self.earliest = Some(key_of(span.start_time_ns, span));
            self.earliest_service.clone_from(&span.service);
        }
        let outcome = semconv::outcome(&span.attributes);
        if self.last.is_none() || !key_less(span.end_time_ns, span, &self.last) {
            self.last = Some(key_of(span.end_time_ns, span));
            self.last_error = error;
            self.last_outcome.clone_from(&outcome);
        }
        if let Some(text) = outcome {
            let later = match &self.declared {
                None => true,
                Some((key, _)) => !key_less(span.end_time_ns, span, &Some(key.clone())),
            };
            if later {
                self.declared = Some((key_of(span.end_time_ns, span), text));
            }
        }
        let attributes = &span.attributes;
        if let Some(environment) = fingerprint::environment(attributes) {
            let id = tables.environments.intern(&environment);
            if !self.environments.contains(&id)
                && self.environments.len() < MAX_ENVIRONMENTS_PER_RUN
            {
                self.environments.push(id);
            }
        }
        if let Some(version) = fingerprint::service_version(attributes) {
            let index = Dimension::ServiceVersion.index();
            let id = tables.values[index].intern(version);
            self.values[index].add(id);
        }
        if let Some(sdk) = fingerprint::sdk_version(attributes) {
            let index = Dimension::SdkVersion.index();
            let id = tables.values[index].intern(sdk);
            self.values[index].add(id);
        }
        let (kind, signature, agent) = semconv::step_identity_with(span, llm);
        if !kind.is_step() {
            return;
        }
        self.steps += 1;
        let token = tables.tokens.intern(&signature);
        self.step_keys
            .push((span.start_time_ns, identity_hash(span), token));
        if self.step_keys.len() >= 2 * MAX_RUN_STEPS {
            self.step_keys.sort_unstable();
            self.step_keys.truncate(MAX_RUN_STEPS);
        }
        if key_less(span.start_time_ns, span, &self.first_step) {
            self.first_step = Some(key_of(span.start_time_ns, span));
            self.first_step_agent = agent;
        }
        match kind {
            StepKind::Tool => {
                let name = signature.strip_prefix("tool:").unwrap_or(&signature);
                let id = tables.tools.intern(name);
                if let Some((_, count)) = self.tools.iter_mut().find(|(tool, _)| *tool == id) {
                    *count += 1;
                } else if self.tools.len() < MAX_TOOLS_PER_RUN {
                    self.tools.push((id, 1));
                }
            }
            StepKind::Llm => {
                for (dimension, value) in fingerprint::llm_config(span) {
                    let index = dimension.index();
                    let id = tables.values[index].intern(value);
                    self.values[index].add(id);
                }
            }
            _ => {}
        }
    }
}

/// A 64-bit hash of a span's identity: the tie-break between two steps that
/// start in the same nanosecond.
fn identity_hash(span: &Span) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    span.trace_id.hash(&mut hasher);
    span.span_id.hash(&mut hasher);
    hasher.finish()
}

/// FNV-1a over `parts`, each followed by a `0xff` separator: a hash whose
/// value is fixed by its definition rather than by the standard library's
/// unspecified hasher, so a permutation test seeded from it gives the same
/// p-value on every build and every machine.
fn stable_hash(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0xff)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

fn run_hash(tenant: &str, run: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tenant.hash(&mut hasher);
    run.hash(&mut hasher);
    hasher.finish()
}

/// The fold's state: runs, the eviction record, and the interners.
struct FoldState {
    runs: HashMap<(String, String), RunAcc>,
    /// Runs let go that had a step: their later spans are ignored.
    evicted: HashSet<u64>,
    /// Runs let go before any step of theirs was seen, with the earliest
    /// start seen for each. Not agent runs as far as anyone knows, so letting
    /// them go does not move the watermark — unless a step of theirs turns up
    /// later, which makes them agent runs with missing spans.
    stepless: HashMap<u64, u64>,
    /// The highest first start of an agent run let go.
    watermark: Option<u64>,
    overflow: bool,
    spans: u64,
    max_runs: usize,
    tables: Tables,
}

impl FoldState {
    fn visit(&mut self, store: &Store, span: &Span) {
        if self.overflow {
            return;
        }
        self.spans += 1;
        let llm = store.facts(span);
        let run = semconv::run_id(&span.attributes, &span.trace_id);
        let key = (span.tenant.clone(), run);
        if !self.runs.contains_key(&key) {
            if self.watermark.is_some() || !self.stepless.is_empty() {
                let hash = run_hash(&key.0, &key.1);
                if self.evicted.contains(&hash) {
                    return;
                }
                if let Some(first) = self.stepless.get(&hash).copied() {
                    let first = first.min(span.start_time_ns);
                    if semconv::step_kind_with(span, &llm).is_step() {
                        // An agent run after all, missing the spans already
                        // let go: it starts at or before `first`, so the
                        // watermark moves past it and it is dropped whole.
                        self.stepless.remove(&hash);
                        self.raise(first);
                        self.forget(hash);
                    } else {
                        self.stepless.insert(hash, first);
                    }
                    return;
                }
                // A run first seen at or before the watermark starts there
                // or earlier, so it would be dropped anyway — and its later
                // spans must not resurrect it as a shorter run.
                if self
                    .watermark
                    .is_some_and(|mark| span.start_time_ns <= mark)
                {
                    self.forget(hash);
                    return;
                }
            }
            self.runs.insert(key.clone(), RunAcc::default());
        }
        let FoldState { runs, tables, .. } = self;
        if let Some(acc) = runs.get_mut(&key) {
            acc.absorb(span, &llm, tables);
        }
        if self.runs.len() > self.max_runs + self.max_runs / 4 + 1 {
            self.evict();
        }
    }

    fn raise(&mut self, start: u64) {
        self.watermark = Some(self.watermark.map_or(start, |mark| mark.max(start)));
    }

    fn forget(&mut self, hash: u64) {
        self.evicted.insert(hash);
        self.check_overflow();
    }

    fn check_overflow(&mut self) {
        if self.evicted.len() + self.stepless.len() > MAX_EVICTED_RUNS {
            self.overflow = true;
        }
    }

    /// Lets runs go, down to the cap: runs with no step first (they are not
    /// agent runs, and a store of plain request traces must not crowd agent
    /// runs out), then the agent runs that started earliest. Every one is
    /// remembered so its later spans are ignored, and every agent run that
    /// truly starts after the resulting watermark is still held whole; see
    /// [`Store::changes`] for why that makes the cut exact.
    fn evict(&mut self) {
        let mut order: Vec<(bool, u64, (String, String))> = self
            .runs
            .iter()
            .map(|(key, acc)| (acc.steps > 0, acc.first_start(), key.clone()))
            .collect();
        order.sort();
        let excess = self.runs.len().saturating_sub(self.max_runs);
        for (has_steps, start, key) in order.into_iter().take(excess) {
            self.runs.remove(&key);
            let hash = run_hash(&key.0, &key.1);
            if has_steps {
                self.raise(start);
                self.forget(hash);
            } else {
                self.stepless.insert(hash, start);
                self.check_overflow();
            }
        }
    }
}

// ------------------------------------------------------------------ analyse

/// A finished run, as the analysis reads it.
struct RunView {
    tenant: String,
    run: String,
    agent: String,
    start_ns: u64,
    complete: bool,
    outcome: Outcome,
    errored: bool,
    cost: f64,
    steps: u32,
    duration_ns: u64,
    path: u32,
    tools: Vec<(u32, u32)>,
    values: [Option<u32>; 7],
    environments: Vec<u32>,
}

/// One dimension's run-level values: the set of step values a run carried.
#[derive(Default)]
struct ComboTable {
    ids: HashMap<(Vec<u32>, bool), u32>,
    members: Vec<(Vec<u32>, bool)>,
}

impl ComboTable {
    fn intern(&mut self, set: &ValueSet) -> u32 {
        let mut ids = set.ids.clone();
        ids.sort_unstable();
        let key = (ids, set.overflow);
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.members.len() as u32;
        self.members.push(key.clone());
        self.ids.insert(key, id);
        id
    }
}

/// Everything value-shaped the analysis renders from.
struct Context<'a> {
    tables: &'a Tables,
    combos: &'a [ComboTable],
    paths: &'a Interner,
    threshold: f64,
}

impl Context<'_> {
    fn display(&self, dimension: Dimension, combo: u32) -> String {
        let index = dimension.index();
        let Some((members, overflow)) = self.combos[index].members.get(combo as usize) else {
            return "(other)".to_owned();
        };
        let mut names: Vec<&str> = members
            .iter()
            .map(|id| self.tables.values[index].display(*id))
            .collect();
        names.sort_unstable();
        let mut text = names.join(" + ");
        if *overflow {
            text.push_str(" + …");
        }
        text
    }

    /// The sample of a combo that is one value.
    fn sample(&self, dimension: Dimension, combo: u32) -> Option<&Sample> {
        let index = dimension.index();
        let (members, overflow) = self.combos[index].members.get(combo as usize)?;
        match (members.as_slice(), overflow) {
            ([only], false) => self.tables.values[index].sample(*only),
            _ => None,
        }
    }
}

/// A change point before its deltas.
struct RawChange {
    dimension: Dimension,
    /// Index of the first run on the new value, within the cohort.
    onset: usize,
    from: u32,
    to: u32,
    shift: Option<Shift>,
}

impl Store {
    /// The change radar over `tenant`'s runs (`None` is the operator view:
    /// every tenant, each its own cohorts), per [`ChangeQuery`]. `now_ns`
    /// dates the session-idle rule (which runs are still active).
    ///
    /// One fold of the window under the query deadline, then the analysis
    /// the module describes, under the same budget. Only agent runs — runs
    /// with at least one llm, tool or agent step — are analysed.
    ///
    /// **The run cap is exact, not approximate.** When the window holds more
    /// than `max_runs` runs, the fold lets runs go: first runs with no step
    /// so far (plain request traces must not crowd agent runs out), then the
    /// agent runs that started earliest. Every run let go is remembered, so
    /// none of its later spans can resurrect it as a shorter run; a step-less
    /// run whose step turns up later is an agent run missing spans, and moves
    /// the watermark past its start. An agent run is only ever let go when no
    /// agent run still held started before it, so every agent run that truly
    /// starts after the watermark is held whole; the runs at or before it are
    /// then dropped too, and the report says where the analysed window
    /// begins (`covered_since_ns`). Runs let go are remembered by a 64-bit
    /// hash, at most a million of them; past that the request is refused.
    pub fn changes(
        &self,
        tenant: Option<&str>,
        query: &ChangeQuery,
        now_ns: u64,
    ) -> Result<ChangeReport> {
        if query.since_ns > query.until_ns {
            return Err(Error::Invalid("since must not be after until".to_owned()));
        }
        if !(query.behavior_threshold > 0.0 && query.behavior_threshold <= 1.0) {
            return Err(Error::Invalid(
                "behavior_threshold must be in (0, 1]".to_owned(),
            ));
        }
        let deadline = self.query_deadline(Instant::now());
        let filter = SpanFilter {
            tenant: tenant.map(str::to_owned),
            since_ns: Some(query.since_ns),
            until_ns: Some(query.until_ns),
            ..SpanFilter::default()
        };
        let mut state = FoldState {
            runs: HashMap::new(),
            evicted: HashSet::new(),
            stepless: HashMap::new(),
            watermark: None,
            overflow: false,
            spans: 0,
            max_runs: query.max_runs.max(1),
            tables: Tables::new(),
        };
        self.fold_spans_under(&filter, deadline, |span| state.visit(self, span))?;
        if state.overflow {
            return Err(Error::Invalid(format!(
                "the window holds more than {MAX_EVICTED_RUNS} runs beyond the run cap; \
                 narrow since/until"
            )));
        }
        let FoldState {
            runs,
            watermark,
            spans,
            tables,
            ..
        } = state;
        let mut truncated = watermark.is_some();

        // Finish every run: its path, its outcome, its value sets.
        let mut paths = Interner::new(usize::MAX);
        let mut combos: Vec<ComboTable> = Dimension::CONFIG
            .iter()
            .map(|_| ComboTable::default())
            .collect();
        let mut views: Vec<RunView> = Vec::with_capacity(runs.len());
        for ((run_tenant, run), mut acc) in runs {
            // A run with no step is not an agent run; one at or before the
            // watermark may be missing spans.
            if acc.steps == 0 || watermark.is_some_and(|mark| acc.first_start() <= mark) {
                continue;
            }
            Deadline::check(deadline, 0)?;
            let agent = acc
                .first_step_agent
                .take()
                .unwrap_or_else(|| acc.earliest_service.clone());
            let ended = run_outcome(&acc, now_ns);
            acc.step_keys.sort_unstable();
            acc.step_keys.truncate(MAX_RUN_STEPS);
            let path = paths.intern(&top_path(&acc, &tables.tokens));
            let mut values = [None; 7];
            for dimension in Dimension::CONFIG {
                let index = dimension.index();
                let set = &acc.values[index];
                if !set.ids.is_empty() {
                    values[index] = Some(combos[index].intern(set));
                }
            }
            let start_ns = acc.first_start();
            let end_ns = acc.last.as_ref().map_or(start_ns, |key| key.0);
            views.push(RunView {
                tenant: run_tenant,
                run,
                agent,
                start_ns,
                complete: ended.reason != "still_active",
                outcome: ended.outcome,
                errored: acc.errors > 0,
                cost: acc.cost,
                steps: acc.steps,
                duration_ns: end_ns.saturating_sub(start_ns),
                path,
                tools: acc.tools,
                values,
                environments: acc.environments,
            });
        }

        // A named run narrows the analysis to its own cohort.
        let named: Option<&RunView> = match &query.run {
            None => None,
            Some(id) => {
                let mut found = views.iter().filter(|view| view.run == *id);
                let first = found.next();
                if first.is_some() && found.next().is_some() {
                    return Err(Error::Invalid(format!(
                        "run {id} exists in more than one tenant; pass tenant= to choose one"
                    )));
                }
                match first {
                    Some(view) => Some(view),
                    None => {
                        Deadline::check(deadline, 0)?;
                        return Ok(ChangeReport {
                            since_ns: query.since_ns,
                            until_ns: query.until_ns,
                            covered_since_ns: watermark.map(|mark| mark.saturating_add(1)),
                            spans_scanned: spans,
                            runs_scanned: 0,
                            agents: Vec::new(),
                            run: None,
                            truncated,
                        });
                    }
                }
            }
        };

        // Filter, then group into cohorts in run-start order.
        let environment = match &query.environment {
            None => None,
            Some(name) => Some(tables.environments.ids.get(name).copied()),
        };
        let mut cohorts: BTreeMap<(String, String), Vec<&RunView>> = BTreeMap::new();
        for view in &views {
            if query
                .agent
                .as_ref()
                .is_some_and(|agent| *agent != view.agent)
            {
                continue;
            }
            if named.is_some_and(|run| run.tenant != view.tenant || run.agent != view.agent) {
                continue;
            }
            match environment {
                Some(None) => continue,
                Some(Some(id)) if !view.environments.contains(&id) => continue,
                _ => {}
            }
            cohorts
                .entry((view.tenant.clone(), view.agent.clone()))
                .or_default()
                .push(view);
        }
        let mut cohorts: Vec<((String, String), Vec<&RunView>)> = cohorts.into_iter().collect();
        if cohorts.len() > MAX_AGENTS {
            cohorts.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
            cohorts.truncate(MAX_AGENTS);
            truncated = true;
        }
        let context = Context {
            tables: &tables,
            combos: &combos,
            paths: &paths,
            threshold: query.behavior_threshold,
        };
        let mut agents = Vec::with_capacity(cohorts.len());
        let mut runs_scanned = 0;
        for ((cohort_tenant, agent), mut runs) in cohorts {
            runs.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then_with(|| a.run.cmp(&b.run)));
            runs_scanned += runs.len();
            agents.push(analyse(&cohort_tenant, &agent, &runs, &context, deadline)?);
        }
        agents.sort_by(|a, b| {
            let lead = |agent: &AgentChanges| {
                agent
                    .changes
                    .first()
                    .filter(|change| change.verdict == Verdict::Regression)
                    .map_or(0.0, |change| change.effect_size)
            };
            lead(b)
                .partial_cmp(&lead(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.changes.len().cmp(&a.changes.len()))
                .then(b.runs.cmp(&a.runs))
                .then_with(|| a.tenant.cmp(&b.tenant))
                .then_with(|| a.agent.cmp(&b.agent))
        });
        // A named run that survived the filters is placed on its timeline.
        let run = named.and_then(|view| {
            agents
                .iter()
                .find(|agent| agent.tenant == view.tenant && agent.agent == view.agent)
                .map(|agent| run_context(view, agent, &context))
        });
        // The final check: an analysis that finished past its budget is
        // refused like one stopped at it.
        Deadline::check(deadline, 0)?;
        Ok(ChangeReport {
            since_ns: query.since_ns,
            until_ns: query.until_ns,
            covered_since_ns: watermark.map(|mark| mark.saturating_add(1)),
            spans_scanned: spans,
            runs_scanned,
            agents,
            run,
            truncated,
        })
    }
}

/// One run placed on its agent's timeline: its fingerprint, and the latest
/// change point of each dimension that introduced the value it carries and
/// started at or before it. Regressions first.
fn run_context(view: &RunView, agent: &AgentChanges, context: &Context<'_>) -> RunContext {
    let mut fingerprint = BTreeMap::new();
    for dimension in Dimension::CONFIG {
        if let Some(combo) = view.values[dimension.index()] {
            fingerprint.insert(
                dimension.as_str().to_owned(),
                context.display(dimension, combo),
            );
        }
    }
    fingerprint.insert("path".to_owned(), context.paths.name(view.path).to_owned());
    let mut latest: BTreeMap<Dimension, &Change> = BTreeMap::new();
    for change in &agent.changes {
        if change.at_ns > view.start_ns {
            continue;
        }
        let carries = match change.dimension {
            Dimension::Behavior => true,
            dimension => fingerprint.get(dimension.as_str()) == Some(&change.to),
        };
        if carries
            && latest
                .get(&change.dimension)
                .map_or(true, |kept| change.at_ns > kept.at_ns)
        {
            latest.insert(change.dimension, change);
        }
    }
    let mut downstream_of: Vec<RunChange> = latest
        .into_values()
        .map(|change| RunChange {
            dimension: change.dimension,
            at_ns: change.at_ns,
            to: change.to.clone(),
            verdict: change.verdict,
            explanation: change.explanation.clone(),
        })
        .collect();
    downstream_of.sort_by(|a, b| {
        a.verdict
            .cmp(&b.verdict)
            .then(b.at_ns.cmp(&a.at_ns))
            .then(a.dimension.cmp(&b.dimension))
    });
    RunContext {
        run: view.run.clone(),
        tenant: view.tenant.clone(),
        agent: view.agent.clone(),
        start_ns: view.start_ns,
        outcome: view.outcome,
        errored: view.errored,
        fingerprint,
        downstream_of,
    }
}

/// How a run ended, by [`attribution::session_outcome`] itself: the fold
/// kept the two spans that rule reads — the one that ended last, and the
/// latest one declaring an outcome — so it is handed exactly those.
fn run_outcome(acc: &RunAcc, now_ns: u64) -> attribution::SessionOutcome {
    let mini = |key: &SpanKey, error: bool, outcome: Option<&str>| {
        let mut attributes = Map::new();
        if let Some(text) = outcome {
            attributes.insert(
                semconv::OUTCOME_KEYS[0].to_owned(),
                Value::String(text.to_owned()),
            );
        }
        Span {
            trace_id: key.1.clone(),
            span_id: key.2.clone(),
            tenant: String::new(),
            parent_span_id: None,
            name: String::new(),
            start_time_ns: key.0,
            end_time_ns: key.0,
            status: if error { "error" } else { "ok" }.to_owned(),
            service: String::new(),
            attributes,
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    };
    let mut spans = Vec::with_capacity(2);
    if let Some(last) = &acc.last {
        spans.push(mini(last, acc.last_error, acc.last_outcome.as_deref()));
    }
    if let Some((key, text)) = &acc.declared {
        if Some(key) != acc.last.as_ref() {
            spans.push(mini(key, false, Some(text)));
        }
    }
    attribution::session_outcome(&spans, now_ns, crate::mcp::SESSION_IDLE_NS)
}

/// A run's top-level path: its first [`BEHAVIOR_PATH_DEPTH`] step tokens,
/// consecutive repeats bucketed, then ` > …` when more steps follow.
fn top_path(acc: &RunAcc, tokens: &Interner) -> String {
    let mut buckets: Vec<(u32, usize)> = Vec::new();
    for &(_, _, token) in &acc.step_keys {
        match buckets.last_mut() {
            Some((last, count)) if *last == token => *count += 1,
            _ => buckets.push((token, 1)),
        }
    }
    let kept_all = acc.steps as usize <= acc.step_keys.len();
    let more = buckets.len() > BEHAVIOR_PATH_DEPTH || !kept_all;
    let mut parts: Vec<String> = buckets
        .iter()
        .take(BEHAVIOR_PATH_DEPTH)
        .map(|&(token, count)| {
            let name = tokens.name(token);
            match count {
                1 => name.to_owned(),
                2 => format!("{name}×2"),
                3..=5 => format!("{name}×3-5"),
                _ => format!("{name}×6+"),
            }
        })
        .collect();
    if parts.is_empty() {
        return "(no steps)".to_owned();
    }
    if more {
        parts.push("…".to_owned());
    }
    parts.join(" > ")
}

/// The sliding detection window for `runs` values.
fn detection_window(runs: usize) -> usize {
    (runs / 8).clamp(MIN_DETECTION_WINDOW, MAX_DETECTION_WINDOW)
}

/// Change points in one dimension's value sequence: `(onset, from, to)`,
/// per the module's rule. Pure, so the rule is testable on its own.
fn change_points(values: &[u32], window: usize) -> Vec<(usize, u32, u32)> {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    let mut current: Option<u32> = None;
    let mut current_runs = 0usize;
    let mut points: Vec<(usize, u32, u32)> = Vec::new();
    let margin = (window / 10).max(2);
    for (index, &value) in values.iter().enumerate() {
        *counts.entry(value).or_insert(0) += 1;
        if index >= window {
            let gone = values[index - window];
            if let Some(count) = counts.get_mut(&gone) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&gone);
                }
            }
        }
        if Some(value) == current {
            current_runs += 1;
        }
        let length = (index + 1).min(window);
        // The leader: most runs in the window; ties keep the incumbent, then
        // the smallest id, so the walk is deterministic.
        let Some((&leader, &lead)) = counts.iter().max_by(|a, b| {
            a.1.cmp(b.1)
                .then_with(|| (Some(*a.0) == current).cmp(&(Some(*b.0) == current)))
                .then_with(|| b.0.cmp(a.0))
        }) else {
            continue;
        };
        let Some(incumbent) = current else {
            current = Some(leader);
            current_runs = values[..=index].iter().filter(|v| **v == leader).count();
            continue;
        };
        if leader == incumbent || lead * 2 < length {
            continue;
        }
        let held = counts.get(&incumbent).copied().unwrap_or(0);
        if lead < held + margin {
            continue;
        }
        let start = index + 1 - length;
        let onset = refine(values, start, index, incumbent, leader);
        let leader_runs = values[onset..=index]
            .iter()
            .filter(|v| **v == leader)
            .count();
        if points.is_empty() && current_runs < MIN_ESTABLISHED_RUNS {
            // Still settling: the first value never established itself.
            current = Some(leader);
            current_runs = leader_runs;
            continue;
        }
        if let Some(&(last_onset, last_from, last_to)) = points.last() {
            if last_to == incumbent && last_from == leader && onset.abs_diff(last_onset) < window {
                // An excursion that came back within a window: withdrawn.
                points.pop();
                current = Some(leader);
                current_runs = values[..=index].iter().filter(|v| **v == leader).count();
                continue;
            }
        }
        points.push((onset, incumbent, leader));
        current = Some(leader);
        current_runs = leader_runs;
    }
    // A fresh value at the very end: every run since it appeared carries it,
    // but there are not yet enough of them to fill half a window. A deploy an
    // hour ago is exactly this, and it is the change an operator asks about.
    if let (Some(incumbent), Some(&last)) = (current, values.last()) {
        let tail = values.iter().rev().take_while(|v| **v == last).count();
        if last != incumbent
            && tail >= MIN_ESTABLISHED_RUNS
            && (current_runs >= MIN_ESTABLISHED_RUNS || !points.is_empty())
        {
            points.push((values.len() - tail, incumbent, last));
        }
    }
    points
}

/// The split inside `[start, end]` that best separates `old` (before) from
/// `new` (after), as the index of the first `new` run at or after it.
fn refine(values: &[u32], start: usize, end: usize, old: u32, new: u32) -> usize {
    let total_new = values[start..=end].iter().filter(|v| **v == new).count();
    let (mut old_before, mut new_before) = (0usize, 0usize);
    let mut best = (total_new, start);
    for split in start + 1..=end {
        if values[split - 1] == old {
            old_before += 1;
        }
        if values[split - 1] == new {
            new_before += 1;
        }
        let score = old_before + (total_new - new_before);
        if score > best.0 {
            best = (score, split);
        }
    }
    (best.1..=end)
        .find(|&index| values[index] == new)
        .unwrap_or(best.1)
}

/// The most common value of the last `window` values; ties go to the latest.
fn dominant_tail(values: &[u32], window: usize) -> Option<u32> {
    let tail = &values[values.len().saturating_sub(window)..];
    let counts = stats::counts(tail);
    tail.iter()
        .enumerate()
        .max_by(|(ai, a), (bi, b)| counts[a].cmp(&counts[b]).then(ai.cmp(bi)))
        .map(|(_, value)| *value)
}

/// Whether a dimension's values are too varied to search.
fn is_volatile(values: &[u32]) -> bool {
    if values.len() < VOLATILE_MIN_RUNS {
        return false;
    }
    let counts = stats::counts(values);
    let unique = values.iter().filter(|value| counts[value] == 1).count();
    unique as f64 > VOLATILE_UNIQUE_SHARE * values.len() as f64
}

/// A behaviour change candidate.
struct BehaviorPoint {
    onset: usize,
    shift: Shift,
}

/// Behaviour change points over a cohort's path sequence, per the module's
/// rule: Jensen–Shannon peaks between adjacent windows, at or above the
/// threshold, significant by permutation, and not within a window of a
/// configuration change.
fn behavior_points(
    paths: &[u32],
    config_onsets: &[usize],
    threshold: f64,
    seed: u64,
    deadline: Option<Deadline>,
) -> Result<Vec<BehaviorPoint>> {
    let n = paths.len();
    let window = (n / 6).clamp(MIN_BEHAVIOR_WINDOW, MAX_BEHAVIOR_WINDOW);
    if n < 2 * window {
        return Ok(Vec::new());
    }
    let mut left = stats::counts(&paths[..window]);
    let mut right = stats::counts(&paths[window..2 * window]);
    let bump = |counts: &mut HashMap<u32, usize>, key: u32, up: bool| {
        if up {
            *counts.entry(key).or_insert(0) += 1;
        } else if let Some(count) = counts.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&key);
            }
        }
    };
    let mut scores: Vec<(usize, f64)> = Vec::with_capacity(n);
    for split in window..=n - window {
        if split > window {
            bump(&mut left, paths[split - 1 - window], false);
            bump(&mut left, paths[split - 1], true);
            bump(&mut right, paths[split - 1], false);
            bump(&mut right, paths[split - 1 + window], true);
        }
        let distance = stats::js_distance(&left, window, &right, window).unwrap_or(0.0);
        scores.push((split, distance));
        if split % 512 == 0 {
            Deadline::check(deadline, 0)?;
        }
    }
    let mut candidates: Vec<(usize, f64)> = Vec::new();
    let mut index = 0;
    while index < scores.len() {
        if scores[index].1 < threshold {
            index += 1;
            continue;
        }
        let mut best = scores[index];
        while index < scores.len() && scores[index].1 >= threshold {
            if scores[index].1 > best.1 {
                best = scores[index];
            }
            index += 1;
        }
        candidates.push(best);
    }
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    // A cohort whose paths are noisy can cross the threshold in many short
    // regions; only the strongest few are worth a permutation test each.
    candidates.truncate(MAX_BEHAVIOR_CANDIDATES);
    let mut accepted: Vec<BehaviorPoint> = Vec::new();
    for (split, distance) in candidates {
        if accepted
            .iter()
            .any(|point| point.onset.abs_diff(split) < window)
            || config_onsets
                .iter()
                .any(|onset| onset.abs_diff(split) < window)
        {
            continue;
        }
        Deadline::check(deadline, 0)?;
        let p_value = stats::js_permutation_p(
            &paths[split - window..split],
            &paths[split..split + window],
            distance,
            BEHAVIOR_PERMUTATIONS,
            seed ^ (split as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        );
        if p_value < BEHAVIOR_ALPHA {
            accepted.push(BehaviorPoint {
                onset: split,
                shift: Shift {
                    js_distance: distance,
                    p_value,
                },
            });
        }
    }
    Ok(accepted)
}

/// One cohort's analysis.
fn analyse(
    tenant: &str,
    agent: &str,
    runs: &[&RunView],
    context: &Context<'_>,
    deadline: Option<Deadline>,
) -> Result<AgentChanges> {
    let mut raw: Vec<RawChange> = Vec::new();
    let mut volatile = Vec::new();
    let mut current = BTreeMap::new();
    for dimension in Dimension::CONFIG {
        let index = dimension.index();
        let sequence: Vec<(usize, u32)> = runs
            .iter()
            .enumerate()
            .filter_map(|(position, run)| run.values[index].map(|combo| (position, combo)))
            .collect();
        if sequence.is_empty() {
            continue;
        }
        let values: Vec<u32> = sequence.iter().map(|(_, combo)| *combo).collect();
        if is_volatile(&values) {
            volatile.push(dimension);
            continue;
        }
        let window = detection_window(values.len());
        if let Some(now) = dominant_tail(&values, window) {
            current.insert(
                dimension.as_str().to_owned(),
                context.display(dimension, now),
            );
        }
        for (onset, from, to) in change_points(&values, window) {
            raw.push(RawChange {
                dimension,
                onset: sequence[onset].0,
                from,
                to,
                shift: None,
            });
        }
    }
    Deadline::check(deadline, 0)?;
    let paths: Vec<u32> = runs.iter().map(|run| run.path).collect();
    let config_onsets: Vec<usize> = raw.iter().map(|change| change.onset).collect();
    let seed = stable_hash(&[tenant, agent]);
    for point in behavior_points(&paths, &config_onsets, context.threshold, seed, deadline)? {
        raw.push(RawChange {
            dimension: Dimension::Behavior,
            onset: point.onset,
            from: OTHER,
            to: OTHER,
            shift: Some(point.shift),
        });
    }

    let mut changes = Vec::with_capacity(raw.len());
    for (position, change) in raw.iter().enumerate() {
        Deadline::check(deadline, 0)?;
        changes.push(build_change(position, change, &raw, runs, context, seed));
    }
    changes.sort_by(|a, b| {
        a.verdict
            .cmp(&b.verdict)
            .then(
                b.effect_size
                    .partial_cmp(&a.effect_size)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.at_ns.cmp(&a.at_ns))
            .then(a.dimension.cmp(&b.dimension))
    });
    let truncated = changes.len() > MAX_CHANGES_PER_AGENT;
    changes.truncate(MAX_CHANGES_PER_AGENT);
    let summary = summarize(agent, runs.len(), &changes, &volatile);
    Ok(AgentChanges {
        agent: agent.to_owned(),
        tenant: tenant.to_owned(),
        runs: runs.len(),
        first_run_ns: runs.first().map_or(0, |run| run.start_ns),
        last_run_ns: runs.last().map_or(0, |run| run.start_ns),
        summary,
        current,
        volatile,
        changes,
        truncated,
    })
}

/// One change point with its sides, deltas, verdict and detail.
fn build_change(
    position: usize,
    change: &RawChange,
    raw: &[RawChange],
    runs: &[&RunView],
    context: &Context<'_>,
    seed: u64,
) -> Change {
    let onset = change.onset;
    let mut co_changes: Vec<Dimension> = raw
        .iter()
        .enumerate()
        .filter(|(other, candidate)| {
            *other != position
                && candidate.dimension != change.dimension
                && candidate.onset.abs_diff(onset) <= CO_CHANGE_RUNS
        })
        .map(|(_, candidate)| candidate.dimension)
        .collect();
    co_changes.sort();
    co_changes.dedup();
    // Sides stop at the neighbouring change points, co-changes excepted.
    let previous = raw
        .iter()
        .map(|candidate| candidate.onset)
        .filter(|&other| other + CO_CHANGE_RUNS < onset)
        .max()
        .unwrap_or(0);
    let next = raw
        .iter()
        .map(|candidate| candidate.onset)
        .filter(|&other| other > onset + CO_CHANGE_RUNS)
        .min()
        .unwrap_or(runs.len());
    let on_value = |run: &RunView, value: u32| match change.dimension {
        Dimension::Behavior => true,
        dimension => run.values[dimension.index()] == Some(value),
    };
    let mut before: Vec<&RunView> = runs[previous..onset]
        .iter()
        .copied()
        .filter(|run| run.complete && on_value(run, change.from))
        .collect();
    if before.len() > MAX_SIDE_RUNS {
        before.drain(..before.len() - MAX_SIDE_RUNS);
    }
    let mut after: Vec<&RunView> = runs[onset..next]
        .iter()
        .copied()
        .filter(|run| run.complete && on_value(run, change.to))
        .collect();
    after.truncate(MAX_SIDE_RUNS);

    let (deltas, tests) = deltas(&before, &after, context);
    let (verdict, effect_size, explanation) = verdict(&before, &after, &tests, &deltas);

    let shift = change.shift.or_else(|| {
        (before.len() >= MIN_RUNS_PER_SIDE && after.len() >= MIN_RUNS_PER_SIDE).then(|| {
            let left: Vec<u32> = before
                .iter()
                .rev()
                .take(SHIFT_SIDE_RUNS)
                .map(|run| run.path)
                .collect();
            let right: Vec<u32> = after
                .iter()
                .take(SHIFT_SIDE_RUNS)
                .map(|run| run.path)
                .collect();
            let distance = stats::js_of(&left, &right).unwrap_or(0.0);
            let p_value = stats::js_permutation_p(
                &left,
                &right,
                distance,
                BEHAVIOR_PERMUTATIONS,
                seed ^ (onset as u64).rotate_left(17) ^ change.dimension.index() as u64,
            );
            Shift {
                js_distance: distance,
                p_value,
            }
        })
    });

    let (from, to, detail) = match change.dimension {
        Dimension::Behavior => {
            let (from, to, rows) = path_detail(&before, &after, context);
            (from, to, Some(json!({ "paths": rows })))
        }
        dimension => (
            context.display(dimension, change.from),
            context.display(dimension, change.to),
            value_detail(
                context.sample(dimension, change.from),
                context.sample(dimension, change.to),
            ),
        ),
    };
    Change {
        at_ns: runs[onset].start_ns,
        dimension: change.dimension,
        from,
        to,
        runs_before: before.len(),
        runs_after: after.len(),
        deltas,
        verdict,
        effect_size,
        explanation,
        co_changes,
        behavior_shift: shift,
        detail,
        example_runs: ExampleRuns {
            before: before
                .iter()
                .rev()
                .take(EXAMPLE_RUNS)
                .map(|run| run.run.clone())
                .collect(),
            after: after
                .iter()
                .take(EXAMPLE_RUNS)
                .map(|run| run.run.clone())
                .collect(),
        },
    }
}

/// One metric's test, for the verdict.
struct MetricTest {
    p_value: f64,
    /// Whether the move was in the bad direction.
    worse: bool,
    /// Signed effect size, after − before.
    effect: f64,
    /// Error and success rates decide first.
    quality: bool,
    /// A phrase for the explanation: "error rate 4.0% → 19.0%".
    phrase: String,
}

fn rate(count: usize, of: usize) -> Option<f64> {
    (of > 0).then(|| count as f64 / of as f64)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn deltas(
    before: &[&RunView],
    after: &[&RunView],
    context: &Context<'_>,
) -> (Deltas, Vec<MetricTest>) {
    let mut tests = Vec::new();
    // Error rate: runs with at least one error span.
    let errored = |side: &[&RunView]| side.iter().filter(|run| run.errored).count();
    let (e1, e2) = (errored(before), errored(after));
    let mut error_rate = RateDelta {
        before: rate(e1, before.len()),
        after: rate(e2, after.len()),
        n_before: before.len(),
        n_after: after.len(),
        ..RateDelta::default()
    };
    if let Some(test) = stats::two_proportion(e1, before.len(), e2, after.len()) {
        error_rate.p_value = Some(test.p_value);
        let (b, a) = (
            error_rate.before.unwrap_or(0.0),
            error_rate.after.unwrap_or(0.0),
        );
        tests.push(MetricTest {
            p_value: test.p_value,
            worse: a > b,
            effect: stats::cohens_h(b, a),
            quality: true,
            phrase: format!("error rate {} → {}", percent(b), percent(a)),
        });
    }
    // Success rate: of the runs that ended at all.
    let decided = |side: &[&RunView]| {
        let ended: Vec<&&RunView> = side
            .iter()
            .filter(|run| run.outcome != Outcome::Unknown)
            .collect();
        let succeeded = ended
            .iter()
            .filter(|run| run.outcome == Outcome::Success)
            .count();
        (succeeded, ended.len())
    };
    let ((s1, d1), (s2, d2)) = (decided(before), decided(after));
    let mut success_rate = RateDelta {
        before: rate(s1, d1),
        after: rate(s2, d2),
        n_before: d1,
        n_after: d2,
        ..RateDelta::default()
    };
    if let Some(test) = stats::two_proportion(s1, d1, s2, d2) {
        success_rate.p_value = Some(test.p_value);
        let (b, a) = (
            success_rate.before.unwrap_or(0.0),
            success_rate.after.unwrap_or(0.0),
        );
        tests.push(MetricTest {
            p_value: test.p_value,
            worse: a < b,
            effect: stats::cohens_h(b, a),
            quality: true,
            phrase: format!("success rate {} → {}", percent(b), percent(a)),
        });
    }
    // Cost: means over every run; the test over priced runs, on the log.
    let costs = |side: &[&RunView]| side.iter().map(|run| run.cost).collect::<Vec<f64>>();
    let (c1, c2) = (costs(before), costs(after));
    let priced = |values: &[f64]| {
        values
            .iter()
            .filter(|cost| **cost > 0.0)
            .map(|cost| cost.ln())
            .collect::<Vec<f64>>()
    };
    let cost_per_run = mean_delta(
        &c1,
        &c2,
        &priced(&c1),
        &priced(&c2),
        &mut tests,
        |b, a| format!("cost per run ${b:.4} → ${a:.4}"),
        c1.iter().chain(&c2).any(|cost| *cost > 0.0),
    );
    let steps = |side: &[&RunView]| {
        side.iter()
            .map(|run| f64::from(run.steps))
            .collect::<Vec<f64>>()
    };
    let (t1, t2) = (steps(before), steps(after));
    let log1p = |values: &[f64]| values.iter().map(|v| v.ln_1p()).collect::<Vec<f64>>();
    let steps_per_run = mean_delta(
        &t1,
        &t2,
        &log1p(&t1),
        &log1p(&t2),
        &mut tests,
        |b, a| format!("steps per run {b:.1} → {a:.1}"),
        true,
    );
    let durations = |side: &[&RunView]| {
        side.iter()
            .map(|run| run.duration_ns as f64)
            .collect::<Vec<f64>>()
    };
    let (u1, u2) = (durations(before), durations(after));
    let log_ms = |values: &[f64]| {
        values
            .iter()
            .map(|v| (v / 1e6).ln_1p())
            .collect::<Vec<f64>>()
    };
    let duration_ns_per_run = mean_delta(
        &u1,
        &u2,
        &log_ms(&u1),
        &log_ms(&u2),
        &mut tests,
        |b, a| format!("duration per run {} → {}", seconds(b), seconds(a)),
        true,
    );
    let deltas = Deltas {
        error_rate,
        success_rate,
        cost_per_run,
        steps_per_run,
        duration_ns_per_run,
        tool_mix: tool_mix(before, after, context),
    };
    (deltas, tests)
}

/// A mean delta, pushing its test (on `log_before` / `log_after`) when it
/// can be computed. `meaningful` false (no run on either side was priced)
/// reports nothing at all rather than a confident zero.
#[allow(clippy::too_many_arguments)]
fn mean_delta(
    before: &[f64],
    after: &[f64],
    log_before: &[f64],
    log_after: &[f64],
    tests: &mut Vec<MetricTest>,
    phrase: impl Fn(f64, f64) -> String,
    meaningful: bool,
) -> MeanDelta {
    if !meaningful {
        return MeanDelta {
            n_before: before.len(),
            n_after: after.len(),
            ..MeanDelta::default()
        };
    }
    let (b, a) = (mean(before), mean(after));
    let mut delta = MeanDelta {
        before: b,
        after: a,
        ratio: match (b, a) {
            (Some(b), Some(a)) if b > 0.0 => Some(a / b),
            _ => None,
        },
        p_value: None,
        n_before: before.len(),
        n_after: after.len(),
    };
    if let Some(test) = stats::welch(log_before, log_after) {
        delta.p_value = Some(test.p_value);
        tests.push(MetricTest {
            p_value: test.p_value,
            worse: test.statistic > 0.0,
            effect: stats::cohens_d(log_before, log_after),
            quality: false,
            phrase: phrase(b.unwrap_or(0.0), a.unwrap_or(0.0)),
        });
    }
    delta
}

fn tool_mix(before: &[&RunView], after: &[&RunView], context: &Context<'_>) -> Vec<ToolShare> {
    let totals = |side: &[&RunView]| {
        let mut counts: HashMap<u32, u64> = HashMap::new();
        let mut total = 0u64;
        for run in side {
            for &(tool, count) in &run.tools {
                *counts.entry(tool).or_insert(0) += u64::from(count);
                total += u64::from(count);
            }
        }
        (counts, total)
    };
    let ((b, bt), (a, at)) = (totals(before), totals(after));
    let share = |counts: &HashMap<u32, u64>, total: u64, tool: u32| {
        if total == 0 {
            0.0
        } else {
            counts.get(&tool).copied().unwrap_or(0) as f64 / total as f64
        }
    };
    let mut tools: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    tools.sort_unstable();
    tools.dedup();
    let mut rows: Vec<ToolShare> = tools
        .into_iter()
        .map(|tool| ToolShare {
            tool: context.tables.tools.name(tool).to_owned(),
            before_share: share(&b, bt, tool),
            after_share: share(&a, at, tool),
        })
        .collect();
    rows.sort_by(|x, y| {
        let weight = |row: &ToolShare| row.before_share.max(row.after_share);
        weight(y)
            .partial_cmp(&weight(x))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.tool.cmp(&y.tool))
    });
    rows.truncate(TOOL_MIX_ROWS);
    rows
}

/// The verdict, its effect size and its sentence.
fn verdict(
    before: &[&RunView],
    after: &[&RunView],
    tests: &[MetricTest],
    deltas: &Deltas,
) -> (Verdict, f64, String) {
    if before.len() < MIN_RUNS_PER_SIDE || after.len() < MIN_RUNS_PER_SIDE {
        return (
            Verdict::InsufficientData,
            0.0,
            format!(
                "{} before and {} after; a verdict needs at least {MIN_RUNS_PER_SIDE} finished \
                 runs on each side",
                count(before.len(), "finished run"),
                count(after.len(), "finished run"),
            ),
        );
    }
    let p_values: Vec<f64> = tests.iter().map(|test| test.p_value).collect();
    let significant = stats::holm(&p_values, VERDICT_ALPHA);
    let chosen = |quality: bool, worse: bool| -> Vec<&MetricTest> {
        tests
            .iter()
            .zip(&significant)
            .filter(|(test, keep)| **keep && test.quality == quality && test.worse == worse)
            .map(|(test, _)| test)
            .collect()
    };
    let (quality_worse, quality_better) = (chosen(true, true), chosen(true, false));
    let (efficiency_worse, efficiency_better) = (chosen(false, true), chosen(false, false));
    let (verdict, deciding, others): (Verdict, Vec<&MetricTest>, Vec<&MetricTest>) =
        if !quality_worse.is_empty() {
            let mut others = quality_better.clone();
            others.extend(efficiency_better.iter().copied());
            (Verdict::Regression, quality_worse, others)
        } else if !quality_better.is_empty() {
            (
                Verdict::Improvement,
                quality_better,
                efficiency_worse.clone(),
            )
        } else if !efficiency_worse.is_empty() {
            (
                Verdict::Regression,
                efficiency_worse,
                efficiency_better.clone(),
            )
        } else if !efficiency_better.is_empty() {
            (Verdict::Improvement, efficiency_better, Vec::new())
        } else {
            (Verdict::Neutral, Vec::new(), Vec::new())
        };
    let effect = deciding
        .iter()
        .map(|test| test.effect.abs())
        .fold(0.0, f64::max);
    let phrase = |test: &&MetricTest| format!("{} ({})", test.phrase, p_text(test.p_value));
    let explanation = match verdict {
        Verdict::Neutral => {
            let error = &deltas.error_rate;
            match (error.before, error.after) {
                (Some(b), Some(a)) => format!(
                    "no significant change: error rate {} → {}{}, and cost, steps and duration \
                     held",
                    percent(b),
                    percent(a),
                    error
                        .p_value
                        .map(|p| format!(" ({})", p_text(p)))
                        .unwrap_or_default(),
                ),
                _ => "no significant change in any metric".to_owned(),
            }
        }
        _ => {
            let mut text = deciding.iter().map(phrase).collect::<Vec<_>>().join("; ");
            if !others.is_empty() {
                let label = if verdict == Verdict::Regression {
                    "though"
                } else {
                    "at a price"
                };
                text.push_str(&format!(
                    " — {label}: {}",
                    others.iter().map(phrase).collect::<Vec<_>>().join("; ")
                ));
            }
            text
        }
    };
    (verdict, effect, explanation)
}

/// The from/to paths of a behaviour change and the paths whose share moved.
fn path_detail(
    before: &[&RunView],
    after: &[&RunView],
    context: &Context<'_>,
) -> (String, String, Vec<Value>) {
    let shares = |side: &[&RunView]| {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for run in side {
            *counts.entry(run.path).or_insert(0) += 1;
        }
        (counts, side.len().max(1) as f64)
    };
    let ((b, bt), (a, at)) = (shares(before), shares(after));
    let top = |counts: &HashMap<u32, usize>| {
        counts
            .iter()
            .max_by(|x, y| x.1.cmp(y.1).then(y.0.cmp(x.0)))
            .map_or_else(String::new, |(path, _)| {
                context.paths.name(*path).to_owned()
            })
    };
    let mut paths: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    let mut rows: Vec<(f64, String, f64, f64)> = paths
        .into_iter()
        .map(|path| {
            let before_share = b.get(&path).copied().unwrap_or(0) as f64 / bt;
            let after_share = a.get(&path).copied().unwrap_or(0) as f64 / at;
            (
                (after_share - before_share).abs(),
                context.paths.name(path).to_owned(),
                before_share,
                after_share,
            )
        })
        .collect();
    rows.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.1.cmp(&y.1))
    });
    rows.truncate(PATH_ROWS);
    let rows = rows
        .into_iter()
        .map(|(_, path, before_share, after_share)| {
            json!({"path": path, "before_share": before_share, "after_share": after_share})
        })
        .collect();
    (top(&b), top(&a), rows)
}

/// What a configuration change's two samples say beyond their identities.
fn value_detail(before: Option<&Sample>, after: Option<&Sample>) -> Option<Value> {
    match (before?, after?) {
        (Sample::Text(old), Sample::Text(new)) => {
            let (offset, context, removed, added) =
                fingerprint::first_divergence(old, new, DIFF_CHARS)?;
            Some(json!({
                "diff": {
                    "offset": offset,
                    "context": context,
                    "before": removed,
                    "after": added,
                },
                "before_chars": old.chars().count(),
                "after_chars": new.chars().count(),
            }))
        }
        (Sample::Names(old), Sample::Names(new)) => {
            let added: Vec<&String> = new.iter().filter(|name| !old.contains(name)).collect();
            let removed: Vec<&String> = old.iter().filter(|name| !new.contains(name)).collect();
            Some(json!({
                "added": added,
                "removed": removed,
                "before_count": old.len(),
                "after_count": new.len(),
                "definitions_changed": added.is_empty() && removed.is_empty(),
            }))
        }
        (Sample::Params(old), Sample::Params(new)) => {
            let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
            keys.sort();
            keys.dedup();
            let changed: Vec<Value> = keys
                .into_iter()
                .filter(|key| old.get(*key) != new.get(*key))
                .map(|key| json!({"key": key, "before": old.get(key), "after": new.get(key)}))
                .collect();
            Some(json!({ "changed": changed }))
        }
        _ => None,
    }
}

fn summarize(agent: &str, runs: usize, changes: &[Change], volatile: &[Dimension]) -> String {
    let volatile_note = if volatile.is_empty() {
        String::new()
    } else {
        format!(
            " Not searched (too varied per run): {}.",
            volatile
                .iter()
                .map(|dimension| dimension.noun())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let Some(top) = changes.first() else {
        return format!(
            "{agent}: no configuration or behaviour change across {}.{volatile_note}",
            count(runs, "run")
        );
    };
    // The most recent change, which ranking may have put anywhere: a deploy
    // an hour ago is `insufficient_data`, and it is the one an operator is
    // asking about.
    let latest = changes
        .iter()
        .max_by(|a, b| a.at_ns.cmp(&b.at_ns).then(b.dimension.cmp(&a.dimension)))
        .unwrap_or(top);
    let headline = match top.verdict {
        Verdict::Regression | Verdict::Improvement => {
            let mut text = format!(
                "{agent}: {} at {}: {}; {}.",
                top.verdict.as_str(),
                crate::mcp::rfc3339(top.at_ns),
                change_phrase(top),
                top.explanation
            );
            if latest.at_ns > top.at_ns {
                text.push_str(&format!(
                    " Since then: {} ({}).",
                    change_phrase(latest),
                    verdict_phrase(latest.verdict)
                ));
            }
            text
        }
        _ => format!(
            "{agent}: {} across {}, none a significant regression; most recent: {} ({}).",
            count(changes.len(), "change"),
            count(runs, "run"),
            change_phrase(latest),
            verdict_phrase(latest.verdict),
        ),
    };
    format!("{headline}{volatile_note}")
}

/// "system prompt changed (a → b), together with the service version".
fn change_phrase(change: &Change) -> String {
    let with = if change.co_changes.is_empty() {
        String::new()
    } else {
        format!(
            ", together with the {}",
            change
                .co_changes
                .iter()
                .map(|dimension| dimension.noun())
                .collect::<Vec<_>>()
                .join(" and the ")
        )
    };
    match change.dimension {
        Dimension::Behavior => format!("behaviour shifted with no configuration change{with}"),
        dimension => format!(
            "{} changed ({} → {}){with}",
            dimension.noun(),
            change.from,
            change.to
        ),
    }
}

fn verdict_phrase(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Regression => "a regression",
        Verdict::Improvement => "an improvement",
        Verdict::Neutral => "no significant effect",
        Verdict::InsufficientData => "too early to judge",
    }
}

fn percent(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn seconds(nanos: f64) -> String {
    format!("{:.2}s", nanos / 1e9)
}

fn p_text(p: f64) -> String {
    if p < 0.001 {
        "p<0.001".to_owned()
    } else {
        format!("p={p:.3}")
    }
}

fn count(value: usize, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

// --------------------------------------------------------------------- http

/// What the route answers.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Value,
    /// Whether the request exhausted the query deadline, for the server's
    /// `traza_query_deadline_exceeded_total`.
    pub deadline_exceeded: bool,
}

impl Reply {
    fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": message.into()}),
            deadline_exceeded: false,
        }
    }
}

/// Whether `path` is this module's route.
pub fn handles(path: &str) -> bool {
    path == ROUTE
}

/// Serves `GET /v1/changes`. `bound` is the credential's tenant binding;
/// the server has already authorized the method (a read).
///
/// Parameters: `agent`, `environment`, `since` and `until` (Unix ns;
/// default the thirty days up to now), `behavior_threshold` (`(0, 1]`),
/// `run` (place one run on its agent's timeline; `404` when it is not among
/// the window's agent runs), and `tenant` (an unbound caller's scope; absent
/// is every tenant). Unknown or repeated parameters are `400`; a bound
/// caller naming another tenant is `403 {"error", "required": "unbound"}`.
pub fn serve(
    store: &Store,
    method: &str,
    path: &str,
    query: &str,
    bound: Option<&str>,
    now_ns: u64,
) -> Reply {
    if !handles(path) {
        return Reply::error(404, "not found");
    }
    if method != "GET" {
        return Reply::error(404, "not found");
    }
    match parse(query, bound, now_ns) {
        Err(reply) => reply,
        Ok((tenant, request)) => match store.changes(tenant.as_deref(), &request, now_ns) {
            Ok(report) if report.run.is_none() && request.run.is_some() => Reply::error(
                404,
                format!(
                    "run not found among the window's agent runs: {}",
                    request.run.as_deref().unwrap_or_default()
                ),
            ),
            Ok(report) => Reply {
                status: 200,
                body: serde_json::to_value(report).unwrap_or_else(|_| json!({})),
                deadline_exceeded: false,
            },
            Err(error @ Error::DeadlineExceeded(_)) => Reply {
                deadline_exceeded: true,
                ..Reply::error(400, error.to_string())
            },
            Err(Error::Invalid(reason)) => Reply::error(400, reason),
            Err(other) => Reply::error(503, other.to_string()),
        },
    }
}

/// The route's parameters, under the binding rules.
fn parse(
    query: &str,
    bound: Option<&str>,
    now_ns: u64,
) -> std::result::Result<(Option<String>, ChangeQuery), Reply> {
    let mut found: HashMap<String, String> = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !matches!(
            key.as_str(),
            "agent" | "environment" | "since" | "until" | "behavior_threshold" | "tenant" | "run"
        ) {
            return Err(Reply::error(400, format!("unknown query parameter: {key}")));
        }
        if found.insert(key.clone(), percent_decode(value)).is_some() {
            return Err(Reply::error(400, format!("{key} is given twice")));
        }
    }
    let nanos = |name: &str| -> std::result::Result<Option<u64>, Reply> {
        found
            .get(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| Reply::error(400, format!("{name} must be Unix nanoseconds")))
            })
            .transpose()
    };
    let mut request = ChangeQuery::recent(now_ns);
    if let Some(until) = nanos("until")? {
        request.until_ns = until;
        request.since_ns = until.saturating_sub(DEFAULT_WINDOW_NS);
    }
    if let Some(since) = nanos("since")? {
        request.since_ns = since;
    }
    if request.since_ns > request.until_ns {
        return Err(Reply::error(400, "since must not be after until"));
    }
    if let Some(threshold) = found.get("behavior_threshold") {
        match threshold.parse::<f64>() {
            Ok(value) if value > 0.0 && value <= 1.0 => request.behavior_threshold = value,
            _ => {
                return Err(Reply::error(
                    400,
                    "behavior_threshold must be a number in (0, 1]",
                ))
            }
        }
    }
    request.agent = found
        .get("agent")
        .filter(|agent| !agent.is_empty())
        .cloned();
    request.run = found.get("run").filter(|run| !run.is_empty()).cloned();
    request.environment = found
        .get("environment")
        .filter(|environment| !environment.is_empty())
        .cloned();
    let tenant = match (bound, found.get("tenant")) {
        (Some(bound), Some(named)) if named != bound => {
            return Err(Reply {
                status: 403,
                body: json!({
                    "error": "this credential is bound to a different tenant",
                    "required": "unbound",
                }),
                deadline_exceeded: false,
            })
        }
        (Some(bound), _) => Some(bound.to_owned()),
        (None, Some(named)) => {
            if !named.is_empty() && !crate::valid_tenant(named) {
                return Err(Reply::error(
                    400,
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
                ));
            }
            Some(named.clone())
        }
        (None, None) => None,
    };
    Ok((tenant, request))
}

/// `%XX` and `+` decoding, as the server decodes every other query.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                output.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::TempStore;
    use serde_json::json;

    const MINUTE: u64 = 60_000_000_000;
    const BASE: u64 = 1_790_000_000_000_000_000;
    const NOW: u64 = BASE + 30 * 86_400_000_000_000;

    /// One synthetic run: a model call, its tool calls, a closing model call
    /// that fails when `failed`.
    struct RunSpec<'a> {
        tenant: &'a str,
        id: String,
        start: u64,
        prompt: &'a str,
        version: &'a str,
        tools: &'a [&'a str],
        failed: bool,
        environment: Option<&'a str>,
    }

    impl RunSpec<'_> {
        fn spans(&self) -> Vec<Span> {
            let base = |span_id: &str, offset: u64, mut attributes: Value| {
                attributes["session.id"] = json!(self.id);
                attributes["service.version"] = json!(self.version);
                if let Some(environment) = self.environment {
                    attributes["deployment.environment.name"] = json!(environment);
                }
                Span {
                    trace_id: format!("trace-{}", self.id),
                    span_id: span_id.to_owned(),
                    tenant: self.tenant.to_owned(),
                    parent_span_id: None,
                    name: span_id.to_owned(),
                    start_time_ns: self.start + offset,
                    end_time_ns: self.start + offset + 5,
                    status: "ok".to_owned(),
                    service: "support-bot".to_owned(),
                    attributes: attributes.as_object().cloned().unwrap_or_default(),
                    events: Vec::new(),
                    links: Vec::new(),
                    extra: Map::new(),
                }
            };
            let llm = || {
                json!({
                    "gen_ai.operation.name": "chat",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.response.model": "gpt-4o-2024-08-06",
                    "gen_ai.request.temperature": 0.2,
                    "llm.cost_usd": 0.01,
                    "gen_ai.input.messages": json!([
                        {"role": "system", "content": self.prompt},
                        {"role": "user", "content": "help"}
                    ]).to_string(),
                })
            };
            let mut spans = vec![base("a-llm", 0, llm())];
            for (index, tool) in self.tools.iter().enumerate() {
                spans.push(base(
                    &format!("b-tool-{index}"),
                    10 + index as u64 * 10,
                    json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": tool}),
                ));
            }
            let mut last = base("c-llm", 500, llm());
            if self.failed {
                last.status = "error".to_owned();
            }
            spans.push(last);
            spans
        }
    }

    fn query() -> ChangeQuery {
        ChangeQuery {
            agent: None,
            environment: None,
            since_ns: BASE - MINUTE,
            until_ns: NOW,
            behavior_threshold: DEFAULT_BEHAVIOR_THRESHOLD,
            max_runs: DEFAULT_MAX_RUNS,
            run: None,
        }
    }

    /// The `(prompt, version, tools)` of one era.
    type Era<'a> = (&'a str, &'a str, &'a [&'a str]);

    /// 60 runs on `before`, then 60 on `after`, failing where the predicates
    /// say (indexes run 0..120 across both eras).
    fn two_eras(
        store: &Store,
        tenant: &str,
        before: Era<'_>,
        after: Era<'_>,
        fail_before: impl Fn(usize) -> bool,
        fail_after: impl Fn(usize) -> bool,
    ) {
        let mut spans = Vec::new();
        for index in 0..120 {
            let ((prompt, version, tools), failed) = if index < 60 {
                (before, fail_before(index))
            } else {
                (after, fail_after(index))
            };
            spans.extend(
                RunSpec {
                    tenant,
                    id: format!("{tenant}-run-{index:03}"),
                    start: BASE + index as u64 * MINUTE,
                    prompt,
                    version,
                    tools,
                    failed,
                    environment: None,
                }
                .spans(),
            );
        }
        store.ingest_batch(spans).expect("ingests");
    }

    fn find(report: &ChangeReport, dimension: Dimension) -> Option<&Change> {
        report
            .agents
            .iter()
            .flat_map(|agent| &agent.changes)
            .find(|change| change.dimension == dimension)
    }

    #[test]
    fn a_prompt_edit_that_raised_the_error_rate_is_the_top_ranked_regression() {
        let fixture = TempStore::open("radar-regression", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            (
                "You help customers. Always answer in English. Today is 2026-10-01.",
                "1.4.2",
                &["search"],
            ),
            (
                "You help customers. Reply in the user's language. Today is 2026-10-09.",
                "1.5.0",
                &["search"],
            ),
            |index| index % 20 == 0,
            |index| index % 5 < 2,
        );
        let report = store.changes(None, &query(), NOW).expect("analyses");
        assert_eq!(report.agents.len(), 1, "{report:#?}");
        let agent = &report.agents[0];
        assert_eq!(agent.agent, "support-bot");
        assert_eq!(agent.runs, 120);
        assert!(!report.truncated);
        let top = &agent.changes[0];
        assert_eq!(top.verdict, Verdict::Regression, "{top:#?}");
        let prompt = find(&report, Dimension::SystemPrompt).expect("the prompt edit is found");
        assert_eq!(
            prompt.at_ns,
            BASE + 60 * MINUTE,
            "placed at the first new run"
        );
        assert_eq!(prompt.verdict, Verdict::Regression);
        assert_eq!(prompt.co_changes, [Dimension::ServiceVersion]);
        assert_eq!((prompt.runs_before, prompt.runs_after), (60, 60));
        let error = &prompt.deltas.error_rate;
        assert_eq!(error.before, Some(3.0 / 60.0));
        assert_eq!(error.after, Some(24.0 / 60.0));
        assert!(error.p_value.is_some_and(|p| p < 0.001), "{error:?}");
        assert!(prompt.effect_size > 0.5);
        assert!(
            prompt.explanation.contains("error rate 5.0% → 40.0%"),
            "{}",
            prompt.explanation
        );
        // The diff names the edit, not the date — dates are volatile.
        let diff = &prompt.detail.as_ref().expect("a diff")["diff"];
        assert_eq!(diff["before"], "Always answer in English");
        assert_eq!(diff["after"], "Reply in the user's language");
        assert_eq!(prompt.example_runs.before[0], "-run-059");
        assert_eq!(prompt.example_runs.after[0], "-run-060");
        let version = find(&report, Dimension::ServiceVersion).expect("the deploy is found");
        assert_eq!(
            (version.from.as_str(), version.to.as_str()),
            ("1.4.2", "1.5.0")
        );
        assert_eq!(version.co_changes, [Dimension::SystemPrompt]);
        assert_eq!(agent.current["service_version"], "1.5.0");
        assert!(agent.summary.contains("regression at"), "{}", agent.summary);
        assert!(
            find(&report, Dimension::Behavior).is_none(),
            "the paths did not change"
        );
        // Unchanged dimensions report no change.
        assert!(find(&report, Dimension::ModelRequested).is_none());
        assert!(find(&report, Dimension::Params).is_none());
    }

    #[test]
    fn a_named_run_is_placed_downstream_of_the_changes_it_ran_on() {
        let fixture = TempStore::open("radar-run", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Always escalate.", "1.4.2", &["search"]),
            ("Never escalate.", "1.5.0", &["search"]),
            |index| index % 20 == 0,
            |index| index % 5 < 2,
        );
        // A run of a second agent, which a named run must not drag in.
        let mut other = RunSpec {
            tenant: "",
            id: "billing-1".to_owned(),
            start: BASE,
            prompt: "Bill.",
            version: "1.0",
            tools: &[],
            failed: false,
            environment: None,
        }
        .spans();
        for span in &mut other {
            span.service = "billing-bot".to_owned();
        }
        store.ingest_batch(other).expect("ingests");
        let after = ChangeQuery {
            run: Some("-run-061".to_owned()),
            ..query()
        };
        let report = store.changes(None, &after, NOW).expect("analyses");
        assert_eq!(report.agents.len(), 1, "narrowed to the run's own cohort");
        let run = report.run.expect("placed");
        assert_eq!(run.agent, "support-bot");
        assert_eq!(run.outcome, Outcome::Failure);
        assert_eq!(run.fingerprint["service_version"], "1.5.0");
        assert_eq!(
            run.fingerprint["path"],
            "llm:gpt-4o-2024-08-06 > tool:search > llm:gpt-4o-2024-08-06"
        );
        let dimensions: Vec<Dimension> = run.downstream_of.iter().map(|c| c.dimension).collect();
        assert_eq!(
            dimensions,
            [Dimension::SystemPrompt, Dimension::ServiceVersion]
        );
        assert!(run
            .downstream_of
            .iter()
            .all(|change| change.verdict == Verdict::Regression
                && change.at_ns == BASE + 60 * MINUTE));
        // A run from before the change is downstream of nothing.
        let before = ChangeQuery {
            run: Some("-run-010".to_owned()),
            ..query()
        };
        let run = store
            .changes(None, &before, NOW)
            .expect("analyses")
            .run
            .expect("placed");
        assert!(run.downstream_of.is_empty(), "{run:#?}");
        assert_eq!(run.fingerprint["service_version"], "1.4.2");
        // An unknown run is no cohort and no placement.
        let unknown = ChangeQuery {
            run: Some("nope".to_owned()),
            ..query()
        };
        let report = store.changes(None, &unknown, NOW).expect("analyses");
        assert!(report.run.is_none() && report.agents.is_empty());
        let reply = serve(store, "GET", ROUTE, "run=nope", None, NOW);
        assert_eq!(reply.status, 404, "{:?}", reply.body);
        let since = BASE - MINUTE;
        let reply = serve(
            store,
            "GET",
            ROUTE,
            &format!("run=-run-061&since={since}&until={NOW}"),
            None,
            NOW,
        );
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        assert_eq!(
            reply.body["run"]["downstream_of"][0]["verdict"],
            "regression"
        );
    }

    #[test]
    fn a_run_id_two_tenants_share_needs_a_tenant() {
        let fixture = TempStore::open("radar-run-tenants", crate::Config::default());
        let store = fixture.store();
        for tenant in ["acme", "globex"] {
            store
                .ingest_batch(
                    RunSpec {
                        tenant,
                        id: "shared".to_owned(),
                        start: BASE,
                        prompt: "Prompt.",
                        version: "1.0",
                        tools: &[],
                        failed: false,
                        environment: None,
                    }
                    .spans(),
                )
                .expect("ingests");
        }
        let shared = ChangeQuery {
            run: Some("shared".to_owned()),
            ..query()
        };
        assert!(matches!(
            store.changes(None, &shared, NOW),
            Err(Error::Invalid(_))
        ));
        let scoped = store
            .changes(Some("globex"), &shared, NOW)
            .expect("analyses");
        assert_eq!(scoped.run.expect("placed").tenant, "globex");
    }

    #[test]
    fn errors_a_run_recovered_from_still_count_against_the_change() {
        // After the change, two runs in five hit a failing tool call and
        // recover: every run still ends in success, so only the error rate
        // can see it.
        let fixture = TempStore::open("radar-recovered", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..120u64 {
            let mut run = RunSpec {
                tenant: "",
                id: format!("run-{index:03}"),
                start: BASE + index * MINUTE,
                prompt: "Prompt.",
                version: if index < 60 { "1.0" } else { "2.0" },
                tools: &["search"],
                failed: false,
                environment: None,
            }
            .spans();
            if index >= 60 && index % 5 < 2 {
                run[1].status = "error".to_owned();
            }
            spans.extend(run);
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert_eq!(version.verdict, Verdict::Regression, "{version:#?}");
        assert_eq!(version.deltas.success_rate.before, Some(1.0));
        assert_eq!(version.deltas.success_rate.after, Some(1.0));
        assert_eq!(version.deltas.error_rate.after, Some(0.4));
        assert!(version.explanation.starts_with("error rate 0.0% → 40.0%"));
        assert!(!version.explanation.contains("success rate"));
    }

    #[test]
    fn each_side_compares_only_runs_on_its_own_value() {
        // A canary on the new version before the change, and stragglers on
        // the old one after it: neither belongs to the side it sits in.
        let fixture = TempStore::open("radar-sides", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..120u64 {
            let version = match index {
                0..=59 if index % 20 == 10 => "2.0",
                0..=59 => "1.0",
                _ if index % 5 == 0 => "1.0",
                _ => "2.0",
            };
            spans.extend(
                RunSpec {
                    tenant: "",
                    id: format!("run-{index:03}"),
                    start: BASE + index * MINUTE,
                    prompt: "Prompt.",
                    version,
                    tools: &["search"],
                    failed: false,
                    environment: None,
                }
                .spans(),
            );
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert_eq!((version.from.as_str(), version.to.as_str()), ("1.0", "2.0"));
        assert_eq!(
            version.at_ns,
            BASE + 61 * MINUTE,
            "the first new run after the split"
        );
        assert_eq!(version.runs_before, 58, "runs 0..=60 less the 3 canaries");
        assert_eq!(
            version.runs_after, 48,
            "runs 61..=119 less the 11 stragglers"
        );
    }

    #[test]
    fn a_deploy_that_moved_nothing_is_neutral() {
        let fixture = TempStore::open("radar-neutral", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.4.2", &["search"]),
            ("Prompt.", "1.5.0", &["search"]),
            |index| index % 20 == 0,
            |index| index % 20 == 0,
        );
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert_eq!(version.verdict, Verdict::Neutral, "{version:#?}");
        assert_eq!(version.effect_size, 0.0);
        assert!(version.co_changes.is_empty());
        assert!(find(&report, Dimension::SystemPrompt).is_none());
        assert!(report.agents[0]
            .summary
            .contains("none a significant regression"));
    }

    #[test]
    fn paths_that_moved_with_no_config_change_are_a_behaviour_change() {
        let fixture = TempStore::open("radar-behavior", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "1.0", &["browse", "browse"]),
            |_| false,
            |_| false,
        );
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let changes = &report.agents[0].changes;
        assert_eq!(changes.len(), 1, "{changes:#?}");
        let behavior = &changes[0];
        assert_eq!(behavior.dimension, Dimension::Behavior);
        assert_eq!(behavior.at_ns, BASE + 60 * MINUTE);
        let shift = behavior.behavior_shift.expect("a shift");
        assert!((shift.js_distance - 1.0).abs() < 1e-12, "disjoint paths");
        assert!(shift.p_value < BEHAVIOR_ALPHA);
        assert_eq!(
            behavior.from,
            "llm:gpt-4o-2024-08-06 > tool:search > llm:gpt-4o-2024-08-06"
        );
        assert_eq!(
            behavior.to,
            "llm:gpt-4o-2024-08-06 > tool:browse×2 > llm:gpt-4o-2024-08-06"
        );
        assert_eq!(behavior.deltas.steps_per_run.before, Some(3.0));
        assert_eq!(behavior.deltas.steps_per_run.after, Some(4.0));
        assert_eq!(
            behavior.verdict,
            Verdict::Regression,
            "more steps for the same outcome"
        );
        let mix = &behavior.deltas.tool_mix;
        assert_eq!(mix[0].tool, "browse");
        assert_eq!((mix[0].before_share, mix[0].after_share), (0.0, 1.0));
    }

    #[test]
    fn a_config_change_explains_a_path_shift_instead_of_a_behaviour_change() {
        let fixture = TempStore::open("radar-explained", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Use search.", "1.0", &["search"]),
            ("Use browse.", "1.0", &["browse", "browse"]),
            |_| false,
            |_| false,
        );
        let report = store.changes(None, &query(), NOW).expect("analyses");
        assert!(find(&report, Dimension::Behavior).is_none());
        let prompt = find(&report, Dimension::SystemPrompt).expect("found");
        let shift = prompt
            .behavior_shift
            .expect("the prompt change carries the shift");
        assert!(shift.js_distance > 0.99 && shift.p_value < BEHAVIOR_ALPHA);
    }

    #[test]
    fn tenants_never_see_each_other() {
        let fixture = TempStore::open("radar-tenants", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "acme",
            ("Acme secret prompt v1.", "1.0", &["search"]),
            ("Acme secret prompt v2.", "1.0", &["search"]),
            |_| false,
            |_| false,
        );
        two_eras(
            store,
            "globex",
            ("Globex prompt.", "1.0", &["search"]),
            ("Globex prompt.", "1.0", &["search"]),
            |_| false,
            |_| false,
        );
        let operator = store.changes(None, &query(), NOW).expect("analyses");
        let tenants: Vec<&str> = operator.agents.iter().map(|a| a.tenant.as_str()).collect();
        assert_eq!(tenants.len(), 2);
        assert!(tenants.contains(&"acme") && tenants.contains(&"globex"));
        let globex = store
            .changes(Some("globex"), &query(), NOW)
            .expect("analyses");
        assert_eq!(globex.agents.len(), 1);
        assert_eq!(globex.agents[0].tenant, "globex");
        assert!(globex.agents[0].changes.is_empty());
        assert_eq!(globex.runs_scanned, 120);
        let acme_prompt = find(&operator, Dimension::SystemPrompt)
            .expect("acme's change")
            .to
            .clone();
        let text = serde_json::to_string(&globex).expect("serializes");
        assert!(
            !text.contains(&acme_prompt),
            "no trace of acme in globex's answer"
        );
        assert!(!text.contains("acme"));
        let nobody = store
            .changes(Some("initech"), &query(), NOW)
            .expect("analyses");
        assert!(nobody.agents.is_empty());
        assert_eq!(nobody.spans_scanned, 0);
    }

    #[test]
    fn the_run_cap_keeps_exactly_the_runs_after_its_mark() {
        let fixture = TempStore::open("radar-cap", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "1.0", &["search"]),
            |_| false,
            |_| false,
        );
        let capped = ChangeQuery {
            max_runs: 20,
            ..query()
        };
        let report = store.changes(None, &capped, NOW).expect("analyses");
        assert!(report.truncated);
        let mark = report
            .covered_since_ns
            .expect("says where the window starts");
        let expected = (0..120u64)
            .filter(|index| BASE + index * MINUTE >= mark)
            .count();
        assert_eq!(report.runs_scanned, expected);
        assert!((1..=26).contains(&expected), "{expected}");
        assert_eq!(
            report.agents[0].first_run_ns,
            BASE + (120 - expected as u64) * MINUTE
        );
    }

    fn plain_span(trace: &str, start: u64, attributes: Value) -> Span {
        Span {
            trace_id: trace.to_owned(),
            span_id: format!("{trace}-span"),
            tenant: String::new(),
            parent_span_id: None,
            name: "GET /checkout".to_owned(),
            start_time_ns: start,
            end_time_ns: start + 5,
            status: "ok".to_owned(),
            service: "checkout-api".to_owned(),
            attributes: attributes.as_object().cloned().unwrap_or_default(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    #[test]
    fn plain_traces_do_not_crowd_out_agent_runs() {
        let fixture = TempStore::open("radar-plain", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "2.0", &["search"]),
            |_| false,
            |_| false,
        );
        // Two hundred request traces, all newer than every agent run.
        let plain: Vec<Span> = (0..200u64)
            .map(|index| {
                plain_span(
                    &format!("plain-{index}"),
                    BASE + 121 * MINUTE + index * 1_000,
                    json!({"service.version": "9.9"}),
                )
            })
            .collect();
        store.ingest_batch(plain).expect("ingests");
        let capped = ChangeQuery {
            max_runs: 130,
            ..query()
        };
        let report = store.changes(None, &capped, NOW).expect("analyses");
        assert!(!report.truncated, "no agent run was let go");
        assert_eq!(report.covered_since_ns, None);
        assert_eq!(report.runs_scanned, 120);
        assert_eq!(
            report.agents.len(),
            1,
            "a service with no steps is no agent"
        );
        assert_eq!(report.agents[0].agent, "support-bot");
    }

    #[test]
    fn a_run_whose_step_arrives_after_it_was_let_go_is_dropped_not_shortened() {
        let fixture = TempStore::open("radar-late-step", crate::Config::default());
        let store = fixture.store();
        // Sealed first, so the fold reads it first: each agent run's workflow
        // wrapper (not a step), then newer request traces.
        let mut sealed: Vec<Span> = (0..10u64)
            .map(|index| {
                let mut span = plain_span(
                    &format!("agent-{index}"),
                    BASE + index * MINUTE,
                    json!({"session.id": format!("agent-{index}"),
                           "traceloop.span.kind": "workflow"}),
                );
                span.service = "support-bot".to_owned();
                span
            })
            .collect();
        sealed.extend((0..30u64).map(|index| {
            plain_span(
                &format!("plain-{index}"),
                BASE + (20 + index) * MINUTE,
                json!({}),
            )
        }));
        store.ingest_batch(sealed).expect("ingests");
        store.flush().expect("seals");
        // Buffered, so read last: each agent run's model call.
        let steps: Vec<Span> = (0..10u64)
            .map(|index| {
                let mut span = plain_span(
                    &format!("agent-{index}"),
                    BASE + index * MINUTE + 100,
                    json!({"session.id": format!("agent-{index}"),
                           "gen_ai.operation.name": "chat",
                           "gen_ai.request.model": "m"}),
                );
                span.span_id = format!("agent-{index}-llm");
                span
            })
            .collect();
        store.ingest_batch(steps).expect("ingests");
        let capped = ChangeQuery {
            max_runs: 10,
            ..query()
        };
        let report = store.changes(None, &capped, NOW).expect("analyses");
        // Every agent run's wrapper was let go before its step was seen: the
        // runs are dropped whole and the cut says so, rather than analysed
        // as runs that began at their model call.
        assert!(report.truncated);
        assert_eq!(report.covered_since_ns, Some(BASE + 9 * MINUTE + 1));
        assert_eq!(report.runs_scanned, 0);
        assert!(report.agents.is_empty());
        // With room for every run, all ten are read whole.
        let report = store.changes(None, &query(), NOW).expect("analyses");
        assert!(!report.truncated);
        assert_eq!(report.runs_scanned, 10);
        assert_eq!(
            report.agents[0].first_run_ns, BASE,
            "the wrapper starts the run"
        );
    }

    #[test]
    fn filters_select_agents_and_environments() {
        let fixture = TempStore::open("radar-filters", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..120u64 {
            // Production never changes; staging deploys 2.0 halfway.
            for (environment, version) in [
                ("prod", "1.0"),
                ("staging", if index < 60 { "1.0" } else { "2.0" }),
            ] {
                spans.extend(
                    RunSpec {
                        tenant: "",
                        id: format!("{environment}-{index:03}"),
                        start: BASE + index * MINUTE,
                        prompt: "Prompt.",
                        version,
                        tools: &["search"],
                        failed: false,
                        environment: Some(environment),
                    }
                    .spans(),
                );
            }
        }
        store.ingest_batch(spans).expect("ingests");
        let all = store.changes(None, &query(), NOW).expect("analyses");
        assert_eq!(all.agents[0].runs, 240);
        let prod = ChangeQuery {
            environment: Some("prod".to_owned()),
            ..query()
        };
        let report = store.changes(None, &prod, NOW).expect("analyses");
        assert_eq!(report.agents[0].runs, 120);
        assert!(report.agents[0].changes.is_empty(), "prod never changed");
        let staging = ChangeQuery {
            environment: Some("staging".to_owned()),
            ..query()
        };
        let report = store.changes(None, &staging, NOW).expect("analyses");
        assert_eq!(
            find(&report, Dimension::ServiceVersion).map(|c| c.to.as_str()),
            Some("2.0")
        );
        let unknown = ChangeQuery {
            environment: Some("qa".to_owned()),
            ..query()
        };
        assert!(store
            .changes(None, &unknown, NOW)
            .expect("analyses")
            .agents
            .is_empty());
        let other_agent = ChangeQuery {
            agent: Some("billing-bot".to_owned()),
            ..query()
        };
        assert!(store
            .changes(None, &other_agent, NOW)
            .expect("analyses")
            .agents
            .is_empty());
    }

    #[test]
    fn a_recent_change_with_few_runs_is_insufficient_data() {
        let fixture = TempStore::open("radar-recent", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..66u64 {
            spans.extend(
                RunSpec {
                    tenant: "",
                    id: format!("run-{index:03}"),
                    start: BASE + index * MINUTE,
                    prompt: "Prompt.",
                    version: if index < 60 { "1.0" } else { "1.1" },
                    tools: &["search"],
                    failed: index >= 60,
                    environment: None,
                }
                .spans(),
            );
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert_eq!(version.verdict, Verdict::InsufficientData);
        assert_eq!(version.runs_after, 6);
        assert!(
            version.explanation.contains("at least 10"),
            "{}",
            version.explanation
        );
        // The deltas are still reported, for a reader to weigh.
        assert_eq!(version.deltas.error_rate.after, Some(1.0));
    }

    #[test]
    fn a_fresh_deploy_is_reported_before_it_has_runs_enough_to_judge() {
        let fixture = TempStore::open("radar-fresh", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Always escalate.", "1.5.0", &["search"]),
            ("Never escalate.", "1.5.0", &["search"]),
            |index| index % 20 == 0,
            |index| index % 5 < 2,
        );
        // Five runs on a hotfix, far fewer than half a detection window.
        let hotfix: Vec<Span> = (120..125u64)
            .flat_map(|index| {
                RunSpec {
                    tenant: "",
                    id: format!("-run-{index:03}"),
                    start: BASE + index * MINUTE,
                    prompt: "Never escalate.",
                    version: "1.5.1",
                    tools: &["search"],
                    failed: false,
                    environment: None,
                }
                .spans()
            })
            .collect();
        store.ingest_batch(hotfix).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let hotfix = find(&report, Dimension::ServiceVersion).expect("the hotfix is seen");
        assert_eq!(
            (hotfix.from.as_str(), hotfix.to.as_str()),
            ("1.5.0", "1.5.1")
        );
        assert_eq!(hotfix.at_ns, BASE + 120 * MINUTE);
        assert_eq!(hotfix.verdict, Verdict::InsufficientData);
        assert_eq!(hotfix.runs_after, 5);
        let summary = &report.agents[0].summary;
        assert!(
            summary.starts_with("support-bot: regression at"),
            "{summary}"
        );
        assert!(
            summary.ends_with(
                "Since then: service version changed (1.5.0 → 1.5.1) (too early to judge)."
            ),
            "{summary}"
        );
    }

    #[test]
    fn still_active_runs_stay_out_of_the_comparison() {
        let fixture = TempStore::open("radar-active", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "2.0", &["search"]),
            |_| false,
            |_| false,
        );
        // "Now" is just after the last run: the newest runs are still active.
        let now = BASE + 120 * MINUTE;
        let report = store.changes(None, &query(), now).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert!(version.runs_after < 60, "{}", version.runs_after);
        assert_eq!(version.runs_before, 60);
    }

    #[test]
    fn the_deadline_is_honoured() {
        let config = crate::Config {
            query_deadline: Some(std::time::Duration::from_nanos(1)),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("radar-deadline", config);
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "2.0", &["search"]),
            |_| false,
            |_| false,
        );
        match store.changes(None, &query(), NOW) {
            Err(Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
    }

    #[test]
    fn invalid_questions_are_refused() {
        let fixture = TempStore::open("radar-invalid", crate::Config::default());
        let store = fixture.store();
        let backwards = ChangeQuery {
            since_ns: 10,
            until_ns: 5,
            ..query()
        };
        assert!(matches!(
            store.changes(None, &backwards, NOW),
            Err(Error::Invalid(_))
        ));
        for threshold in [0.0, -1.0, 1.5, f64::NAN] {
            let bad = ChangeQuery {
                behavior_threshold: threshold,
                ..query()
            };
            assert!(matches!(
                store.changes(None, &bad, NOW),
                Err(Error::Invalid(_))
            ));
        }
    }

    #[test]
    fn change_points_follow_dominance_not_appearance() {
        let ones = |n: usize, value: u32| vec![value; n];
        let concat = |parts: &[Vec<u32>]| parts.concat();
        // A clean switch is placed at its first run.
        assert_eq!(
            change_points(&concat(&[ones(30, 0), ones(30, 1)]), 8),
            [(30, 0, 1)]
        );
        // Two in a row.
        assert_eq!(
            change_points(&concat(&[ones(30, 0), ones(30, 1), ones(30, 2)]), 10),
            [(30, 0, 1), (60, 1, 2)]
        );
        // A 5% canary never becomes dominant.
        let canary: Vec<u32> = (0..100).map(|i| u32::from(i % 20 == 0)).collect();
        assert!(change_points(&canary, 12).is_empty());
        // An even A/B split is not a change either.
        let split: Vec<u32> = (0..100).map(|i| i % 2).collect();
        assert!(change_points(&split, 12).is_empty());
        // An excursion that comes back within a window is withdrawn.
        let excursion = concat(&[ones(40, 0), ones(6, 1), ones(40, 0)]);
        assert!(
            change_points(&excursion, 10).is_empty(),
            "{:?}",
            change_points(&excursion, 10)
        );
        // A first value that never established itself is settling.
        assert_eq!(
            change_points(&concat(&[ones(1, 5), ones(30, 0), ones(30, 1)]), 8),
            [(31, 0, 1)]
        );
        // A fresh value holding every run at the end is a change before it
        // fills half a window; two runs are not enough to say so.
        assert_eq!(
            change_points(&concat(&[ones(60, 0), ones(5, 1)]), 20),
            [(60, 0, 1)]
        );
        assert!(change_points(&concat(&[ones(60, 0), ones(2, 1)]), 20).is_empty());
        assert!(
            change_points(&concat(&[ones(60, 0), ones(4, 1), ones(1, 0)]), 20).is_empty(),
            "not if the last run went back"
        );
        // A rollout that ramps up is placed where the new value took over.
        let mut ramp = ones(40, 0);
        ramp.extend((0..20).map(|i| u32::from(i % 4 == 0)));
        ramp.extend(ones(40, 1));
        let points = change_points(&ramp, 12);
        assert_eq!(points.len(), 1, "{points:?}");
        assert!((40..=62).contains(&points[0].0), "{points:?}");
    }

    #[test]
    fn top_paths_bucket_repeats_and_mark_continuations() {
        let mut tokens = Interner::new(16);
        let llm = tokens.intern("llm:m");
        let a = tokens.intern("tool:a");
        let b = tokens.intern("tool:b");
        let run = |sequence: &[u32], steps: u32| RunAcc {
            step_keys: sequence
                .iter()
                .enumerate()
                .map(|(index, token)| (index as u64, 0, *token))
                .collect(),
            steps,
            ..RunAcc::default()
        };
        assert_eq!(
            top_path(&run(&[llm, a, a, a, llm, b, llm], 7), &tokens),
            "llm:m > tool:a×3-5 > llm:m > tool:b > …"
        );
        assert_eq!(
            top_path(&run(&[llm, a, a, llm], 4), &tokens),
            "llm:m > tool:a×2 > llm:m"
        );
        assert_eq!(top_path(&run(&[a; 7], 7), &tokens), "tool:a×6+");
        assert_eq!(
            top_path(&run(&[llm, llm], 90), &tokens),
            "llm:m×2 > …",
            "steps beyond those kept are a continuation"
        );
        assert_eq!(top_path(&run(&[], 0), &tokens), "(no steps)");
    }

    #[test]
    fn volatility_is_a_share_of_singletons() {
        let unique: Vec<u32> = (0..40).collect();
        assert!(is_volatile(&unique));
        let stable: Vec<u32> = (0..40).map(|i| i / 10).collect();
        assert!(!is_volatile(&stable));
        assert!(!is_volatile(&unique[..10]), "too few runs to judge");
    }

    #[test]
    fn the_route_parses_binds_and_refuses() {
        let fixture = TempStore::open("radar-route", crate::Config::default());
        let store = fixture.store();
        let ok = serve(store, "GET", ROUTE, "", None, NOW);
        assert_eq!(ok.status, 200, "{:?}", ok.body);
        assert_eq!(ok.body["agents"], json!([]));
        assert_eq!(ok.body["since_ns"], json!(NOW - DEFAULT_WINDOW_NS));
        for (query, status) in [
            ("bogus=1", 400),
            ("agent=a&agent=b", 400),
            ("since=yesterday", 400),
            ("since=10&until=5", 400),
            ("behavior_threshold=0", 400),
            ("behavior_threshold=2", 400),
            ("tenant=Not+Valid", 400),
        ] {
            let reply = serve(store, "GET", ROUTE, query, None, NOW);
            assert_eq!(reply.status, status, "{query}: {:?}", reply.body);
            assert!(reply.body["error"].is_string());
        }
        let foreign = serve(store, "GET", ROUTE, "tenant=globex", Some("acme"), NOW);
        assert_eq!(foreign.status, 403);
        assert_eq!(foreign.body["required"], "unbound");
        let own = serve(store, "GET", ROUTE, "tenant=acme", Some("acme"), NOW);
        assert_eq!(own.status, 200);
        assert_eq!(serve(store, "POST", ROUTE, "", None, NOW).status, 404);
        assert_eq!(
            serve(store, "GET", "/v1/changes/x", "", None, NOW).status,
            404
        );
        let windowed = serve(store, "GET", ROUTE, "until=5000000000000000000", None, NOW);
        assert_eq!(
            windowed.body["since_ns"],
            json!(5_000_000_000_000_000_000u64 - DEFAULT_WINDOW_NS)
        );
    }
}
