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
//! every key). Values are compared by a 64-bit digest; their text is kept
//! only to be shown, in bounded tables.
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
//! `max(2, window/10)` runs; that proposes a change point, placed at the
//! first run of the split that best separates incumbent from newcomer inside
//! that window — for a clean deploy, exactly the first run on the new value.
//! A newcomer that loses dominance again within one window is an excursion
//! and its proposal is withdrawn.
//!
//! A proposal is a change point only once **confirmed** on runs that took no
//! part in proposing it, because a value drawn at random per run — an A/B
//! split, a router's two models, a branch that sometimes calls a guardrail —
//! wins a window by chance all the time. In the window of runs after the
//! newcomer became dominant, the incumbent must be rare (at most a fifth of
//! them) and the newcomer must hold the majority; and before the onset the
//! newcomer must have been rare or the incumbent established (at least
//! three fifths), over at least half a window. A proposal too recent to have
//! that window after it is confirmed only if the newcomer was rare before
//! and has held the majority since, with the incumbent rare. A fresh value
//! that has held every run since it appeared, at the very end of the
//! sequence, and was rare before, is a change point from its third run on —
//! a deploy too recent to fill half a window, and usually
//! `insufficient_data`.
//!
//! A dimension where more than half the runs carry a value no other run
//! shares — typically a prompt embedding something per-request that masking
//! did not catch — or that has more than [`MAX_CHANGES_PER_DIMENSION`]
//! change points is **volatile**, and is reported as such instead of being
//! searched.
//!
//! **Behaviour** changes are the ones no configuration explains: the
//! distribution of the cohort's top-level paths (each run's first
//! [`BEHAVIOR_PATH_DEPTH`] step tokens, consecutive repeats bucketed `x`,
//! `x×2`, `x×3-5`, `x×6+` as the Fate Map buckets them) is compared between
//! adjacent windows by Jensen–Shannon distance — by sample splitting: the
//! even-indexed runs choose candidate splits (distance peaks at or above the
//! threshold, default [`DEFAULT_BEHAVIOR_THRESHOLD`], none within a window of
//! a configuration change), and the odd-indexed runs test them: their own
//! distance must reach the threshold, and a deterministic permutation test
//! must call it significant after Bonferroni's correction for the candidates
//! tested (p < [`BEHAVIOR_ALPHA`]). That holds the chance of reporting any
//! behaviour change in a cohort whose paths never changed at 1%. Such a
//! change is reported as dimension `behavior` — the provider changed under
//! you. Every reported configuration change also carries the distance
//! between its two sides (`behavior_shift`), so "the prompt changed AND the
//! agent now takes different paths" is visible.
//!
//! # Deltas and verdicts
//!
//! Each change compares the runs on its old value just before it with the
//! runs on its new value just after it, each side bounded by the
//! neighbouring change points (co-changes within [`CO_CHANGE_RUNS`] runs
//! excepted) and by [`MAX_SIDE_RUNS`]. Runs still active (last span within
//! the session idle window of the earlier of now and `until`) are left out:
//! their cost and steps may be partial. Rates (error rate: runs with at least
//! one error span; success rate: runs that ended success, of those that
//! ended at all) use the pooled two-proportion z-test, or Fisher's exact test
//! when an expected cell count is below 5 (where the normal approximation
//! overstates significance); cost, steps and duration use Welch's t-test on
//! the logarithm (cost over priced runs only; steps and duration as
//! `ln(1+x)`), so one runaway run does not decide a verdict. A metric takes
//! part in the verdict only when its own test had [`MIN_RUNS_PER_SIDE`]
//! observations on each side — runs that ended, for the success rate; priced
//! runs, for cost — and is reported either way. Holm's correction holds the
//! family-wise error rate across a change's tests at 5%. The verdict is
//! `regression` when a quality metric (error or success rate) got
//! significantly worse — or, with quality unchanged, an efficiency metric
//! did — `improvement` symmetrically, `neutral` when nothing is significant,
//! and `insufficient_data` with fewer than [`MIN_RUNS_PER_SIDE`] runs on a
//! side; `decided_by` says which kind of metric decided. Changes are ranked by verdict (regressions first), then
//! quality-decided before efficiency-decided, then by effect size (Cohen's h
//! for rates, Cohen's d — capped at 4 — for log-scale means), then newest.
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
//! Two passes over one snapshot of the window, and the analysis, under one
//! query deadline. Only agent runs (at least one llm, tool or agent step)
//! are analysed: at most [`max_runs`] of them, the newest by first step (the
//! window then starts later and says so), each holding at most
//! [`MAX_RUN_STEPS`] earliest steps, [`MAX_RUN_VALUES`] values per dimension
//! and clipped strings. Display names are kept for at most
//! [`MAX_VALUES_PER_DIMENSION`] values per dimension, [`MAX_VALUES_PER_OWNER`]
//! of them per service, with [`MAX_SAMPLE_BYTES`] of diff samples; at most
//! [`MAX_AGENTS`] cohorts are reported, with [`MAX_CHANGES_PER_AGENT`]
//! changes each. Tenant-scoped like every read: a bound caller folds its own
//! tenant's spans and nothing else, and every table is keyed by tenant.

mod fingerprint;
mod stats;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::attribution::{self, Outcome};
use crate::semconv::{self, StepKind};
use crate::{Deadline, Error, QueryCost, Result, Span, SpanFilter, Store};

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

/// The chance, per cohort, of reporting a behaviour change in paths that
/// never changed: the level each candidate's Bonferroni-corrected
/// permutation p-value must beat.
pub const BEHAVIOR_ALPHA: f64 = 0.01;

/// Relabellings in a behaviour candidate's permutation test: resolution
/// 1/2000, fine enough for [`BEHAVIOR_ALPHA`] corrected for
/// [`MAX_BEHAVIOR_CANDIDATES`] candidates.
pub const BEHAVIOR_PERMUTATIONS: usize = 1_999;

/// Relabellings in the path-shift test across a configuration change
/// (`behavior_shift`): resolution 1/200.
pub const SHIFT_PERMUTATIONS: usize = 199;

/// Behaviour candidates tested per cohort at most, strongest first.
pub const MAX_BEHAVIOR_CANDIDATES: usize = 16;

/// Change points one dimension of one cohort may have before it is
/// reported as volatile instead: a value that changes this often is not a
/// history of deploys.
pub const MAX_CHANGES_PER_DIMENSION: usize = 100;

/// Family-wise error rate of one change's metric tests (Holm).
pub const VERDICT_ALPHA: f64 = 0.05;

/// Earliest steps kept per run — enough for the top-level path.
pub const MAX_RUN_STEPS: usize = 32;

/// Distinct values of one dimension kept per run; a run carrying more is
/// shown with a trailing ` + …`.
pub const MAX_RUN_VALUES: usize = 4;

/// Distinct values per dimension one analysis keeps a display name (and a
/// diff sample) for; a value past it is shown by its digest. Values are
/// compared by digest, so this bounds what is shown, never what is found.
pub const MAX_VALUES_PER_DIMENSION: usize = 2_048;

/// Of [`MAX_VALUES_PER_DIMENSION`], the names one tenant's service may take,
/// so one agent with a per-request value cannot leave every other unnamed.
pub const MAX_VALUES_PER_OWNER: usize = 256;

/// Bytes of diff samples (prompt texts, catalog tool names, parameters)
/// kept per dimension per analysis.
pub const MAX_SAMPLE_BYTES: usize = 1 << 20;

/// Step-token and tool names kept per analysis, and of those per service.
const MAX_TOKEN_NAMES: usize = 8_192;
const MAX_TOKEN_NAMES_PER_OWNER: usize = 1_024;
/// Bytes of a trace or span id a run keeps for ordering.
const MAX_ID_BYTES: usize = 128;
/// Bytes of a run id kept as is; a longer one is kept as a prefix and a
/// digest of the whole.
const MAX_RUN_ID_BYTES: usize = 256;
/// Bytes of a service, agent, step, tool or environment name kept.
const MAX_NAME_BYTES: usize = 256;
/// Bytes of a declared outcome kept: every word the outcome rule knows is
/// shorter.
const MAX_OUTCOME_BYTES: usize = 32;
const MAX_TOOLS_PER_RUN: usize = 64;
const MAX_ENVIRONMENTS_PER_RUN: usize = 4;
/// llm steps extracted between the read's own deadline checks.
const EXTRACTION_CHECK_INTERVAL: usize = 64;
/// Values the analysis reads between two checks of its deadline.
const PACE_CHECK_WORK: usize = 1 << 16;
const TOOL_MIX_ROWS: usize = 8;
const PATH_ROWS: usize = 5;
const EXAMPLE_RUNS: usize = 3;
const DIFF_CHARS: usize = 80;
/// Runs per side a configuration change's behaviour shift is tested on.
const SHIFT_SIDE_RUNS: usize = 200;
const MIN_BEHAVIOR_WINDOW: usize = 15;
const MAX_BEHAVIOR_WINDOW: usize = 100;
const MIN_DETECTION_WINDOW: usize = 8;
const MAX_DETECTION_WINDOW: usize = 50;
/// Confirming a change point: the share at or below which a value is rare
/// (the incumbent after, the newcomer before) ...
const CONFIRM_RARE_SHARE: f64 = 0.2;
/// ... the share at or above which the incumbent was established before ...
const CONFIRM_ESTABLISHED_SHARE: f64 = 0.6;
/// ... the share the newcomer must hold after ...
const CONFIRM_MAJORITY_SHARE: f64 = 0.5;
/// ... or, in a window of at least [`CONFIRM_DROP_WINDOW`] runs, how far the
/// incumbent's share must fall across the change when it was neither
/// established nor the newcomer rare before (a mixed stage ending).
const CONFIRM_DROP: f64 = 0.3;
const CONFIRM_DROP_WINDOW: usize = 24;
/// A dimension is volatile when this share of its runs carry a value no
/// other run shares (and it has at least [`VOLATILE_MIN_RUNS`] runs).
const VOLATILE_UNIQUE_SHARE: f64 = 0.5;
const VOLATILE_MIN_RUNS: usize = 20;
/// A behaviour change's `from`/`to` combo placeholder (paths are rendered
/// from the sides instead).
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

/// Which kind of metric decided a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Error rate or success rate: what the agent achieved.
    Quality,
    /// Cost, steps or duration: what achieving it cost.
    Efficiency,
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
    /// Two-sided: the pooled two-proportion z-test, or Fisher's exact test
    /// when an expected cell count is below 5; `null` when it cannot be
    /// computed. Reported even when a side has too few runs rated for the
    /// metric to take part in the verdict.
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
    /// Welch's t-test on the log scale, two-sided. For cost, over the priced
    /// runs only.
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
    /// Which kind of metric decided a `regression` or `improvement`: a
    /// quality one (error or success rate) or, with quality unchanged, an
    /// efficiency one. Absent for `neutral` and `insufficient_data`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Basis>,
    /// The effect size that decided the verdict (absolute; 0 when neutral):
    /// Cohen's h for a rate, Cohen's d (capped at 4) for a log-scale mean.
    /// Only comparable between changes the same [`Basis`] decided.
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
    /// Whether a value, tool or path step of this agent is shown as `#` and
    /// a 16-hex digest because its name did not fit the bounded name tables
    /// ([`MAX_VALUES_PER_DIMENSION`], [`MAX_VALUES_PER_OWNER`]). Detection
    /// compares digests, so nothing was missed; only the name is.
    pub unnamed: bool,
}

/// A change analysis.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChangeReport {
    /// Window start asked for, Unix ns.
    pub since_ns: u64,
    /// Window end, Unix ns.
    pub until_ns: u64,
    /// When the run cap applied: every agent run whose first step starts at
    /// or after this instant is analysed in full, and no run whose first
    /// step started before it is.
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
    /// Whether any bound cut the answer: the run cap, the cohort cap, or a
    /// name table (an agent's `unnamed`).
    pub truncated: bool,
}

// --------------------------------------------------------------- accumulate

/// A string interner. The analysis uses it for top-level paths, of which
/// there are at most as many as runs held.
struct Interner {
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    fn new() -> Self {
        Self {
            ids: HashMap::new(),
            names: Vec::new(),
        }
    }

    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.ids.get(text) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(text.to_owned());
        self.ids.insert(text.to_owned(), id);
        id
    }

    fn name(&self, id: u32) -> &str {
        self.names.get(id as usize).map_or("", String::as_str)
    }
}

/// `text` cut to at most `max` bytes, at a character boundary.
fn clipped(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A run's id as the analysis holds it: the id itself, or — past
/// [`MAX_RUN_ID_BYTES`] — its first bytes and a digest of the whole, so a
/// planted megabyte session id costs no more to hold than a short one, and
/// two long ids sharing a prefix stay two runs.
fn run_key(run: String) -> String {
    if run.len() <= MAX_RUN_ID_BYTES {
        return run;
    }
    format!(
        "{}…#{}",
        clipped(&run, MAX_RUN_ID_BYTES - 20),
        fingerprint::digest16(run.as_bytes())
    )
}

/// A value's 64-bit identity, fixed by its definition (FNV-1a) so it is the
/// same on every machine. Runs carry these digests, never the values' text:
/// change detection compares identities only, so no table can overflow it.
/// Every structure keyed by a digest is keyed by the tenant too, so a value
/// one tenant crafted to collide with another's can only confuse its own
/// answer.
fn digest(text: &str) -> u64 {
    stable_hash(&[text])
}

/// The bytes a sample holds.
fn sample_bytes(sample: &Sample) -> usize {
    match sample {
        Sample::Text(text) => text.len(),
        Sample::Names(names) => names.iter().map(String::len).sum(),
        Sample::Params(params) => params
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum(),
    }
}

/// The display names (and diff samples) of digests, bounded three ways: at
/// most `cap` names, at most `quota` from one owner (one tenant's service),
/// so that an agent with a per-request value cannot take the table from
/// every other, and at most [`MAX_SAMPLE_BYTES`] of samples. A digest
/// refused a name keeps its identity, is shown as `#` and its 16 hex digits,
/// and the agent it is shown for says so (`unnamed`).
struct NameTable {
    names: HashMap<(u32, u64), String>,
    samples: HashMap<(u32, u64), Sample>,
    owners: HashMap<(u32, u64), usize>,
    cap: usize,
    quota: usize,
    sample_bytes: usize,
}

impl NameTable {
    fn new(cap: usize, quota: usize) -> Self {
        Self {
            names: HashMap::new(),
            samples: HashMap::new(),
            owners: HashMap::new(),
            cap,
            quota,
            sample_bytes: 0,
        }
    }

    /// Names `value` in `tenant` (with its sample), unless it has a name
    /// already or there is no room for one more from `owner`.
    fn record(&mut self, tenant: u32, value: u64, owner: u64, name: &str, sample: Option<Sample>) {
        let key = (tenant, value);
        if self.names.contains_key(&key) || self.names.len() >= self.cap {
            return;
        }
        let held = self.owners.entry((tenant, owner)).or_insert(0);
        if *held >= self.quota {
            return;
        }
        *held += 1;
        self.names.insert(key, name.to_owned());
        if let Some(sample) = sample {
            let bytes = sample_bytes(&sample);
            if self.sample_bytes + bytes <= MAX_SAMPLE_BYTES {
                self.sample_bytes += bytes;
                self.samples.insert(key, sample);
            }
        }
    }

    fn name(&self, tenant: u32, value: u64) -> Option<&str> {
        self.names.get(&(tenant, value)).map(String::as_str)
    }

    fn sample(&self, tenant: u32, value: u64) -> Option<&Sample> {
        self.samples.get(&(tenant, value))
    }
}

/// How a digest without a name is shown.
fn unnamed(value: u64) -> String {
    format!("#{value:016x}")
}

struct Tables {
    tokens: NameTable,
    tools: NameTable,
    values: Vec<NameTable>,
}

impl Tables {
    fn new() -> Self {
        Self {
            tokens: NameTable::new(MAX_TOKEN_NAMES, MAX_TOKEN_NAMES_PER_OWNER),
            tools: NameTable::new(MAX_TOKEN_NAMES, MAX_TOKEN_NAMES_PER_OWNER),
            values: Dimension::CONFIG
                .iter()
                .map(|_| NameTable::new(MAX_VALUES_PER_DIMENSION, MAX_VALUES_PER_OWNER))
                .collect(),
        }
    }

    /// Records one value of `dimension` and returns its identity.
    fn value(&mut self, dimension: Dimension, tenant: u32, owner: u64, value: Fingerprint) -> u64 {
        let id = digest(&value.key);
        self.values[dimension.index()].record(tenant, id, owner, &value.display, value.sample);
        id
    }
}

/// A run's distinct values of one dimension.
#[derive(Clone, Debug, Default)]
struct ValueSet {
    ids: Vec<u64>,
    overflow: bool,
}

impl ValueSet {
    fn add(&mut self, id: u64) {
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

/// A span's place in the canonical `(time, trace, span)` order, ids clipped
/// to [`MAX_ID_BYTES`] so a planted id cannot make a run expensive to hold.
type SpanKey = (u64, String, String);

fn key_less(time: u64, span: &Span, than: &Option<SpanKey>) -> bool {
    match than {
        None => true,
        Some((t, trace, id)) => {
            (
                time,
                clipped(&span.trace_id, MAX_ID_BYTES),
                clipped(&span.span_id, MAX_ID_BYTES),
            ) < (*t, trace.as_str(), id.as_str())
        }
    }
}

fn key_of(time: u64, span: &Span) -> SpanKey {
    (
        time,
        clipped(&span.trace_id, MAX_ID_BYTES).to_owned(),
        clipped(&span.span_id, MAX_ID_BYTES).to_owned(),
    )
}

/// Everything one run contributes, folded span by span in bounded memory:
/// every string it holds is clipped ([`MAX_ID_BYTES`], [`MAX_NAME_BYTES`],
/// [`MAX_OUTCOME_BYTES`]) and every list capped.
#[derive(Default)]
struct RunAcc {
    earliest: Option<SpanKey>,
    earliest_service: String,
    first_step: Option<SpanKey>,
    first_step_agent: Option<String>,
    last: Option<SpanKey>,
    last_error: bool,
    /// The latest span declaring an outcome, and the declaration — clipped,
    /// which keeps every outcome word [`attribution::session_outcome`]
    /// recognizes intact (the longest is nine bytes).
    declared: Option<(SpanKey, String)>,
    errors: u32,
    cost: f64,
    steps: u32,
    step_keys: Vec<(u64, u64, u64)>,
    tools: Vec<(u64, u32)>,
    values: [ValueSet; 7],
    environments: Vec<u64>,
}

impl RunAcc {
    fn first_start(&self) -> u64 {
        self.earliest.as_ref().map_or(u64::MAX, |key| key.0)
    }

    fn first_step_start(&self) -> u64 {
        self.first_step.as_ref().map_or(u64::MAX, |key| key.0)
    }

    /// Folds one span in; whether it was an llm step, whose configuration
    /// extraction is the expensive part of a read.
    fn absorb(
        &mut self,
        span: &Span,
        llm: &semconv::LlmFacts,
        tables: &mut Tables,
        tenant: u32,
    ) -> bool {
        let error = span.status.eq_ignore_ascii_case("error");
        if error {
            self.errors += 1;
        }
        self.cost += llm.cost_usd.filter(|cost| cost.is_finite()).unwrap_or(0.0);
        let service = clipped(&span.service, MAX_NAME_BYTES);
        if key_less(span.start_time_ns, span, &self.earliest) {
            self.earliest = Some(key_of(span.start_time_ns, span));
            service.clone_into(&mut self.earliest_service);
        }
        if self.last.is_none() || !key_less(span.end_time_ns, span, &self.last) {
            self.last = Some(key_of(span.end_time_ns, span));
            self.last_error = error;
        }
        if let Some(text) = semconv::outcome(&span.attributes) {
            let later = match &self.declared {
                None => true,
                Some((key, _)) => !key_less(span.end_time_ns, span, &Some(key.clone())),
            };
            if later {
                self.declared = Some((
                    key_of(span.end_time_ns, span),
                    clipped(&text, MAX_OUTCOME_BYTES).to_owned(),
                ));
            }
        }
        let attributes = &span.attributes;
        if let Some(environment) = fingerprint::environment(attributes) {
            let id = digest(clipped(&environment, MAX_NAME_BYTES));
            if !self.environments.contains(&id)
                && self.environments.len() < MAX_ENVIRONMENTS_PER_RUN
            {
                self.environments.push(id);
            }
        }
        let owner = digest(service);
        if let Some(version) = fingerprint::service_version(attributes) {
            let id = tables.value(Dimension::ServiceVersion, tenant, owner, version);
            self.values[Dimension::ServiceVersion.index()].add(id);
        }
        if let Some(sdk) = fingerprint::sdk_version(attributes) {
            let id = tables.value(Dimension::SdkVersion, tenant, owner, sdk);
            self.values[Dimension::SdkVersion.index()].add(id);
        }
        let (kind, signature, agent) = semconv::step_identity_with(span, llm);
        if !kind.is_step() {
            return false;
        }
        self.steps += 1;
        let signature = clipped(&signature, MAX_NAME_BYTES);
        let token = digest(signature);
        tables.tokens.record(tenant, token, owner, signature, None);
        self.step_keys
            .push((span.start_time_ns, identity_hash(span), token));
        if self.step_keys.len() >= 2 * MAX_RUN_STEPS {
            self.step_keys.sort_unstable();
            self.step_keys.truncate(MAX_RUN_STEPS);
        }
        if key_less(span.start_time_ns, span, &self.first_step) {
            self.first_step = Some(key_of(span.start_time_ns, span));
            self.first_step_agent = agent.map(|agent| clipped(&agent, MAX_NAME_BYTES).to_owned());
        }
        match kind {
            StepKind::Tool => {
                let name = signature.strip_prefix("tool:").unwrap_or(signature);
                let id = digest(name);
                tables.tools.record(tenant, id, owner, name, None);
                if let Some((_, count)) = self.tools.iter_mut().find(|(tool, _)| *tool == id) {
                    *count += 1;
                } else if self.tools.len() < MAX_TOOLS_PER_RUN {
                    self.tools.push((id, 1));
                }
                false
            }
            StepKind::Llm => {
                for (dimension, value) in fingerprint::llm_config(span) {
                    let id = tables.value(dimension, tenant, owner, value);
                    self.values[dimension.index()].add(id);
                }
                true
            }
            _ => false,
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

/// The read's first pass: which agent runs the analysis holds. Only step
/// spans count, so a store of plain request traces costs this pass nothing
/// to remember.
///
/// Each run is ranked by the start of its first step. When more than twice
/// the cap are held, every run but the newest `max_runs` is let go and the
/// watermark rises to the latest first-step start let go — and every held
/// run starting at or before it is let go too, as is any later step at or
/// before it. A run only ever leaves when its first step is at or before the
/// watermark, so every run whose first step starts after the final
/// watermark is held at the end, and only those are; the second pass reads
/// them whole.
struct Selection {
    firsts: HashMap<(String, String), u64>,
    watermark: Option<u64>,
    max_runs: usize,
}

impl Selection {
    fn new(max_runs: usize) -> Self {
        Self {
            firsts: HashMap::new(),
            watermark: None,
            max_runs,
        }
    }

    fn visit(&mut self, span: &Span) {
        let llm = semconv::facts(&span.attributes);
        if !semconv::step_kind_with(span, &llm).is_step() {
            return;
        }
        let start = span.start_time_ns;
        let key = (
            span.tenant.clone(),
            run_key(semconv::run_id(&span.attributes, &span.trace_id)),
        );
        if self.watermark.is_some_and(|mark| start <= mark) {
            // Its first step is at or before the mark, wherever its others are.
            self.firsts.remove(&key);
            return;
        }
        let first = self.firsts.entry(key).or_insert(start);
        *first = (*first).min(start);
        if self.firsts.len() > 2 * self.max_runs {
            self.prune();
        }
    }

    /// Down to the newest `max_runs` (ties at the cut go too), raising the
    /// watermark.
    fn prune(&mut self) {
        if self.firsts.len() <= self.max_runs {
            return;
        }
        let mut starts: Vec<u64> = self.firsts.values().copied().collect();
        // Newest first: the element at `max_runs` is the newest run let go.
        let (_, mark, _) = starts.select_nth_unstable_by(self.max_runs, |a, b| b.cmp(a));
        let mark = *mark;
        self.watermark = Some(self.watermark.map_or(mark, |old| old.max(mark)));
        self.firsts.retain(|_, start| *start > mark);
    }

    /// The runs held, by tenant, and the watermark.
    fn finish(mut self) -> (HashMap<String, HashSet<String>>, Option<u64>) {
        self.prune();
        let mut held: HashMap<String, HashSet<String>> = HashMap::new();
        for (tenant, run) in self.firsts.into_keys() {
            held.entry(tenant).or_default().insert(run);
        }
        (held, self.watermark)
    }
}

/// The read's second pass: every span of every run the first pass held,
/// folded into its run.
struct Accumulate {
    /// Tenant → (its id in the name tables, run → accumulator).
    runs: HashMap<String, (u32, HashMap<String, RunAcc>)>,
    tables: Tables,
    spans: u64,
    extractions: usize,
    /// The deadline passed during the fold: every later span is skipped, and
    /// the read is refused when the fold returns.
    expired: bool,
    deadline: Option<Deadline>,
}

impl Accumulate {
    fn new(held: HashMap<String, HashSet<String>>, deadline: Option<Deadline>) -> Self {
        let mut tenants: Vec<String> = held.keys().cloned().collect();
        tenants.sort();
        let ids: HashMap<String, u32> = tenants
            .into_iter()
            .enumerate()
            .map(|(id, tenant)| (tenant, id as u32))
            .collect();
        let runs = held
            .into_iter()
            .map(|(tenant, runs)| {
                let id = ids[&tenant];
                let runs = runs
                    .into_iter()
                    .map(|run| (run, RunAcc::default()))
                    .collect();
                (tenant, (id, runs))
            })
            .collect();
        Self {
            runs,
            tables: Tables::new(),
            spans: 0,
            extractions: 0,
            expired: false,
            deadline,
        }
    }

    fn visit(&mut self, store: &Store, span: &Span) {
        if self.expired {
            return;
        }
        self.spans += 1;
        let Accumulate { runs, tables, .. } = self;
        let Some((tenant, runs)) = runs.get_mut(span.tenant.as_str()) else {
            return;
        };
        let run = run_key(semconv::run_id(&span.attributes, &span.trace_id));
        let Some(acc) = runs.get_mut(&run) else {
            return;
        };
        let llm = store.facts(span);
        if acc.absorb(span, &llm, tables, *tenant) {
            // The fold checks its deadline every few thousand records, which
            // suits cheap visitors; an llm step costs a parse, a mask and a
            // digest of its whole request, so this one checks more often.
            self.extractions += 1;
            if self.extractions % EXTRACTION_CHECK_INTERVAL == 0
                && Deadline::check(self.deadline, 0).is_err()
            {
                self.expired = true;
            }
        }
    }
}

// ------------------------------------------------------------------ analyse

/// A finished run, as the analysis reads it.
struct RunView {
    tenant: String,
    tenant_id: u32,
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
    /// Whether a step token of its path is shown by digest.
    unnamed_path: bool,
    tools: Vec<(u64, u32)>,
    values: [Option<u32>; 7],
    environments: Vec<u64>,
}

/// One dimension's run-level values: the set of step values a run carried.
#[derive(Default)]
struct ComboTable {
    ids: HashMap<(Vec<u64>, bool), u32>,
    members: Vec<(Vec<u64>, bool)>,
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

/// The window, read: its runs and everything needed to render them.
struct Window {
    views: Vec<RunView>,
    tables: Tables,
    combos: Vec<ComboTable>,
    paths: Interner,
    spans: u64,
    watermark: Option<u64>,
}

/// Everything value-shaped one cohort renders from.
struct Context<'a> {
    tables: &'a Tables,
    combos: &'a [ComboTable],
    paths: &'a Interner,
    threshold: f64,
    /// The cohort's tenant: every name is looked up in it.
    tenant: u32,
    /// Set when something was shown by digest for want of a name.
    unnamed: Cell<bool>,
}

impl Context<'_> {
    fn value_name(&self, index: usize, value: u64) -> String {
        match self.tables.values[index].name(self.tenant, value) {
            Some(name) => name.to_owned(),
            None => {
                self.unnamed.set(true);
                unnamed(value)
            }
        }
    }

    fn tool_name(&self, tool: u64) -> String {
        match self.tables.tools.name(self.tenant, tool) {
            Some(name) => name.to_owned(),
            None => {
                self.unnamed.set(true);
                unnamed(tool)
            }
        }
    }

    fn display(&self, dimension: Dimension, combo: u32) -> String {
        let index = dimension.index();
        let Some((members, overflow)) = self.combos[index].members.get(combo as usize) else {
            return String::new();
        };
        let mut names: Vec<String> = members
            .iter()
            .map(|value| self.value_name(index, *value))
            .collect();
        names.sort_unstable();
        let mut text = names.join(" + ");
        if *overflow {
            text.push_str(" + …");
        }
        text
    }

    /// The samples a change between two combos can be shown with: the
    /// value each side does not share with the other, when exactly one
    /// value changed. A single-valued run changes its one value; a
    /// multi-agent run whose executor prompt changed keeps its planner prompt
    /// on both sides, and the executor's two prompts are what differ.
    fn changed_samples(
        &self,
        dimension: Dimension,
        from: u32,
        to: u32,
    ) -> (Option<&Sample>, Option<&Sample>) {
        let index = dimension.index();
        let members = |combo: u32| {
            self.combos[index]
                .members
                .get(combo as usize)
                .filter(|(_, overflow)| !overflow)
                .map(|(members, _)| members)
        };
        let (Some(old), Some(new)) = (members(from), members(to)) else {
            return (None, None);
        };
        let removed: Vec<u64> = old.iter().copied().filter(|id| !new.contains(id)).collect();
        let added: Vec<u64> = new.iter().copied().filter(|id| !old.contains(id)).collect();
        match (removed.as_slice(), added.as_slice()) {
            ([before], [after]) => (
                self.tables.values[index].sample(self.tenant, *before),
                self.tables.values[index].sample(self.tenant, *after),
            ),
            _ => (None, None),
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
    /// Two passes over one snapshot of the window, then the analysis the
    /// module describes, all under one query deadline. Only agent runs —
    /// runs with at least one llm, tool or agent step — are analysed.
    ///
    /// **The run cap is exact, not approximate.** The first pass reads only
    /// step spans and ranks each run by the start of its first step; it keeps
    /// the newest `max_runs` and says where the cut fell (the watermark). The
    /// second pass reads every span of exactly those runs — a run's
    /// workflow root, its HTTP server span and its late spans included — so
    /// every agent run whose first step starts after the watermark is
    /// analysed whole, and no other run is. Plain request traces are never
    /// remembered by either pass, so they cannot crowd agent runs out, and no
    /// store is too big to ask about: a bigger window only costs more time,
    /// which the deadline bounds.
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
        let window = self.read_window(tenant, query, now_ns, deadline)?;
        analyse_window(&window, query, deadline)
    }

    /// Both passes of the read, and every run finished.
    fn read_window(
        &self,
        tenant: Option<&str>,
        query: &ChangeQuery,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<Window> {
        let filter = SpanFilter {
            tenant: tenant.map(str::to_owned),
            since_ns: Some(query.since_ns),
            until_ns: Some(query.until_ns),
            ..SpanFilter::default()
        };
        // One snapshot for both passes, so the second reads exactly the
        // store the first ranked.
        let view = self.snapshot()?;
        let mut cost = QueryCost::default();
        let mut selection = Selection::new(query.max_runs.max(1));
        view.fold(&filter, deadline, &mut cost, &mut |span| {
            selection.visit(span)
        })?;
        let (held, watermark) = selection.finish();
        let mut accumulate = Accumulate::new(held, deadline);
        view.fold(&filter, deadline, &mut cost, &mut |span| {
            accumulate.visit(self, span);
        })?;
        if accumulate.expired {
            Deadline::check(deadline, cost.segments_examined)?;
        }
        // A run whose last span is within the idle window of `until` may
        // have gone on past it: judged against the earlier of now and until.
        finish_runs(accumulate, watermark, now_ns.min(query.until_ns), deadline)
    }
}

/// Every held run finished: its agent, outcome, path and value sets, in
/// run-start order (so every id the analysis assigns is deterministic).
fn finish_runs(
    accumulate: Accumulate,
    watermark: Option<u64>,
    active_before_ns: u64,
    deadline: Option<Deadline>,
) -> Result<Window> {
    let Accumulate {
        runs,
        tables,
        spans,
        ..
    } = accumulate;
    let mut held: Vec<(String, u32, String, RunAcc)> = Vec::new();
    for (tenant, (tenant_id, runs)) in runs {
        for (run, acc) in runs {
            // A run whose first step is at or before the watermark is not
            // analysed: the first pass may have let some of it go.
            if acc.steps == 0 || watermark.is_some_and(|mark| acc.first_step_start() <= mark) {
                continue;
            }
            held.push((tenant.clone(), tenant_id, run, acc));
        }
    }
    held.sort_by(|a, b| (a.3.first_start(), &a.0, &a.2).cmp(&(b.3.first_start(), &b.0, &b.2)));
    let mut paths = Interner::new();
    let mut combos: Vec<ComboTable> = Dimension::CONFIG
        .iter()
        .map(|_| ComboTable::default())
        .collect();
    let mut views = Vec::with_capacity(held.len());
    for (position, (tenant, tenant_id, run, mut acc)) in held.into_iter().enumerate() {
        if position % 1_024 == 0 {
            Deadline::check(deadline, 0)?;
        }
        let agent = acc
            .first_step_agent
            .take()
            .unwrap_or_else(|| acc.earliest_service.clone());
        let ended = run_outcome(&acc, active_before_ns);
        acc.step_keys.sort_unstable();
        acc.step_keys.truncate(MAX_RUN_STEPS);
        let (text, unnamed_path) = top_path(&acc, &tables.tokens, tenant_id);
        let path = paths.intern(&text);
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
            tenant,
            tenant_id,
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
            unnamed_path,
            tools: acc.tools,
            values,
            environments: acc.environments,
        });
    }
    Ok(Window {
        views,
        tables,
        combos,
        paths,
        spans,
        watermark,
    })
}

/// The analysis of a read window, under the same deadline as the read.
fn analyse_window(
    window: &Window,
    query: &ChangeQuery,
    deadline: Option<Deadline>,
) -> Result<ChangeReport> {
    Deadline::check(deadline, 0)?;
    let mut truncated = window.watermark.is_some();
    let covered_since_ns = window.watermark.map(|mark| mark.saturating_add(1));
    let environment = query
        .environment
        .as_deref()
        .map(|name| digest(clipped(name, MAX_NAME_BYTES)));

    // A named run narrows the analysis to its own cohort.
    let named: Option<&RunView> = match &query.run {
        None => None,
        Some(id) => {
            let key = run_key(id.clone());
            let mut found = window.views.iter().filter(|view| view.run == key);
            let first = found.next();
            if first.is_some() && found.next().is_some() {
                return Err(Error::Invalid(format!(
                    "run {id} exists in more than one tenant; name its tenant to choose one"
                )));
            }
            let Some(view) = first else {
                return Ok(ChangeReport {
                    since_ns: query.since_ns,
                    until_ns: query.until_ns,
                    covered_since_ns,
                    spans_scanned: window.spans,
                    runs_scanned: 0,
                    agents: Vec::new(),
                    run: None,
                    truncated,
                });
            };
            if query
                .agent
                .as_ref()
                .is_some_and(|agent| *agent != view.agent)
            {
                return Err(Error::Invalid(format!(
                    "run {id} is excluded by the agent filter: it belongs to another agent; \
                     drop agent= to place it"
                )));
            }
            if environment.is_some_and(|wanted| !view.environments.contains(&wanted)) {
                return Err(Error::Invalid(format!(
                    "run {id} is excluded by the environment filter: it does not report that \
                     environment; drop environment= to place it"
                )));
            }
            Some(view)
        }
    };

    // Filter, then group into cohorts in run-start order.
    let mut cohorts: BTreeMap<(String, String), Vec<&RunView>> = BTreeMap::new();
    for view in &window.views {
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
        if environment.is_some_and(|wanted| !view.environments.contains(&wanted)) {
            continue;
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
    let context_for = |tenant: u32| Context {
        tables: &window.tables,
        combos: &window.combos,
        paths: &window.paths,
        threshold: query.behavior_threshold,
        tenant,
        unnamed: Cell::new(false),
    };
    let mut agents = Vec::with_capacity(cohorts.len());
    let mut runs_scanned = 0;
    let mut run = None;
    for ((cohort_tenant, agent), mut runs) in cohorts {
        // Already in start order; the run id breaks a tie the same way.
        runs.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then_with(|| a.run.cmp(&b.run)));
        runs_scanned += runs.len();
        let context = context_for(runs[0].tenant_id);
        let mut changes = analyse(&cohort_tenant, &agent, &runs, &context, deadline)?;
        if let Some(view) = named.filter(|view| view.tenant == cohort_tenant && view.agent == agent)
        {
            run = Some(run_context(view, &changes, &context));
        }
        changes.unnamed = context.unnamed.get() || runs.iter().any(|run| run.unnamed_path);
        truncated |= changes.unnamed;
        agents.push(changes);
    }
    agents.sort_by(|a, b| {
        let lead = |agent: &AgentChanges| {
            agent
                .changes
                .first()
                .filter(|change| change.verdict == Verdict::Regression)
                .map(|change| (basis_rank(change), change.effect_size))
        };
        let led = match (lead(a), lead(b)) {
            (Some(x), Some(y)) => {
                x.0.cmp(&y.0)
                    .then(y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal))
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        led.then(b.changes.len().cmp(&a.changes.len()))
            .then(b.runs.cmp(&a.runs))
            .then_with(|| a.tenant.cmp(&b.tenant))
            .then_with(|| a.agent.cmp(&b.agent))
    });
    // The final check: an analysis that finished past its budget is
    // refused like one stopped at it.
    Deadline::check(deadline, 0)?;
    Ok(ChangeReport {
        since_ns: query.since_ns,
        until_ns: query.until_ns,
        covered_since_ns,
        spans_scanned: window.spans,
        runs_scanned,
        agents,
        run,
        truncated,
    })
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
/// `active_before_ns` is the instant the idle rule measures from.
fn run_outcome(acc: &RunAcc, active_before_ns: u64) -> attribution::SessionOutcome {
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
        // The last span's own declaration, if it made one, is the latest.
        let declared = acc
            .declared
            .as_ref()
            .filter(|(key, _)| key == last)
            .map(|(_, text)| text.as_str());
        spans.push(mini(last, acc.last_error, declared));
    }
    if let Some((key, text)) = &acc.declared {
        if Some(key) != acc.last.as_ref() {
            spans.push(mini(key, false, Some(text)));
        }
    }
    attribution::session_outcome(&spans, active_before_ns, crate::mcp::SESSION_IDLE_NS)
}

/// A run's top-level path: its first [`BEHAVIOR_PATH_DEPTH`] step tokens,
/// consecutive repeats bucketed, then ` > …` when more steps follow; and
/// whether a token in it had no name.
fn top_path(acc: &RunAcc, tokens: &NameTable, tenant: u32) -> (String, bool) {
    let mut buckets: Vec<(u64, usize)> = Vec::new();
    for &(_, _, token) in &acc.step_keys {
        match buckets.last_mut() {
            Some((last, count)) if *last == token => *count += 1,
            _ => buckets.push((token, 1)),
        }
    }
    let kept_all = acc.steps as usize <= acc.step_keys.len();
    let more = buckets.len() > BEHAVIOR_PATH_DEPTH || !kept_all;
    let mut missing = false;
    let mut parts: Vec<String> = buckets
        .iter()
        .take(BEHAVIOR_PATH_DEPTH)
        .map(|&(token, count)| {
            let name = match tokens.name(tenant, token) {
                Some(name) => name.to_owned(),
                None => {
                    missing = true;
                    unnamed(token)
                }
            };
            match count {
                1 => name,
                2 => format!("{name}×2"),
                3..=5 => format!("{name}×3-5"),
                _ => format!("{name}×6+"),
            }
        })
        .collect();
    if parts.is_empty() {
        return ("(no steps)".to_owned(), false);
    }
    if more {
        parts.push("…".to_owned());
    }
    (parts.join(" > "), missing)
}

/// The sliding detection window for `runs` values.
fn detection_window(runs: usize) -> usize {
    (runs / 8).clamp(MIN_DETECTION_WINDOW, MAX_DETECTION_WINDOW)
}

/// The share of `values` equal to `value`; `0` for none.
fn share(values: &[u32], value: u32) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().filter(|v| **v == value).count() as f64 / values.len() as f64
}

/// Work counted against a query deadline: every loop of the analysis over a
/// cohort's runs reports the values it reads, and the deadline is checked
/// whenever another [`PACE_CHECK_WORK`] of them have been read — so no loop,
/// whatever its length, runs far past the budget.
struct Pace {
    deadline: Option<Deadline>,
    /// Values read so far.
    work: usize,
    /// The work at which the deadline is checked next (the first report
    /// checks it).
    next_check: usize,
}

impl Pace {
    fn new(deadline: Option<Deadline>) -> Self {
        Self {
            deadline,
            work: 0,
            next_check: 0,
        }
    }

    /// Reports `units` more values read; refused once the deadline passed.
    fn add(&mut self, units: usize) -> Result<()> {
        self.work = self.work.saturating_add(units);
        if self.work >= self.next_check {
            Deadline::check(self.deadline, 0)?;
            self.next_check = self.work.saturating_add(PACE_CHECK_WORK);
        }
        Ok(())
    }
}

/// A proposed switch, `(onset, from, to, detected)`: `detected` is the index
/// at which the newcomer became dominant.
type Switch = (usize, u32, u32, usize);

/// What the dominance walk proposes: its switches, and the walk's final
/// incumbent with its run count.
struct Walk {
    switches: Vec<Switch>,
    current: Option<u32>,
    current_runs: usize,
}

/// The dominance walk of the module docs, which proposes change points.
///
/// Linear in runs: each run updates the window's counts and the running
/// totals of every value seen so far, so restoring an incumbent after a
/// withdrawn excursion reads its total instead of recounting the prefix; a
/// proposal reads at most two windows more. The walk reports what it reads
/// to `pace` as it goes, so a long sequence stops at the deadline, not after
/// it. Both maps hold at most one entry per distinct value in `values`.
fn dominance_walk(values: &[u32], window: usize, pace: &mut Pace) -> Result<Walk> {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    let mut totals: HashMap<u32, usize> = HashMap::new();
    let mut current: Option<u32> = None;
    let mut current_runs = 0usize;
    let mut switches: Vec<Switch> = Vec::new();
    let margin = (window / 10).max(2);
    for (index, &value) in values.iter().enumerate() {
        *counts.entry(value).or_insert(0) += 1;
        *totals.entry(value).or_insert(0) += 1;
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
        pace.add(1 + counts.len())?;
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
        // Every run of a value up to here: what a restored incumbent holds.
        let so_far = |value: u32| totals.get(&value).copied().unwrap_or(0);
        let Some(incumbent) = current else {
            current = Some(leader);
            current_runs = so_far(leader);
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
        pace.add(2 * length)?;
        if switches.is_empty() && current_runs < MIN_ESTABLISHED_RUNS {
            // Still settling: the first value never established itself.
            current = Some(leader);
            current_runs = leader_runs;
            continue;
        }
        if let Some(&(last_onset, last_from, last_to, _)) = switches.last() {
            if last_to == incumbent && last_from == leader && onset.abs_diff(last_onset) < window {
                // An excursion that came back within a window: withdrawn.
                switches.pop();
                current = Some(leader);
                current_runs = so_far(leader);
                continue;
            }
        }
        switches.push((onset, incumbent, leader, index));
        current = Some(leader);
        current_runs = leader_runs;
    }
    Ok(Walk {
        switches,
        current,
        current_runs,
    })
}

/// Whether a proposed switch is a change rather than a mixture's noise.
///
/// In the window of runs AFTER the newcomer became dominant — runs that took
/// no part in proposing it — the incumbent must be rare and the newcomer
/// must hold the majority. Before the onset (at least half a window of it)
/// the newcomer must have been rare, or the incumbent established, or — in
/// a cohort whose window is at least [`CONFIRM_DROP_WINDOW`] runs, long
/// enough for its shares to mean something — the incumbent's share must
/// have fallen by [`CONFIRM_DROP`]: the end of a gradual rollout that held a
/// mixed stage. A switch too recent to have the window after it is
/// confirmed only if the newcomer was rare before and has held the majority
/// since, with the incumbent rare. `floor` is the previous confirmed onset:
/// history before it belongs to an older value.
fn confirmed(
    values: &[u32],
    (onset, from, to, detected): Switch,
    window: usize,
    floor: usize,
) -> bool {
    let start = onset.saturating_sub(window).max(floor).min(onset);
    let before = &values[start..onset];
    if before.len() < (window / 2).max(MIN_ESTABLISHED_RUNS) {
        return false;
    }
    let fresh = share(before, to) <= CONFIRM_RARE_SHARE;
    let established = share(before, from) >= CONFIRM_ESTABLISHED_SHARE;
    let after_start = (detected + 1).min(values.len());
    let after = &values[after_start..(after_start + window).min(values.len())];
    if after.len() >= window {
        let completed = window >= CONFIRM_DROP_WINDOW
            && share(before, from) - share(after, from) >= CONFIRM_DROP;
        (fresh || established || completed)
            && share(after, from) <= CONFIRM_RARE_SHARE
            && share(after, to) >= CONFIRM_MAJORITY_SHARE
    } else {
        let since = &values[onset..];
        fresh
            && share(since, from) <= CONFIRM_RARE_SHARE
            && share(since, to) >= CONFIRM_MAJORITY_SHARE
    }
}

/// Change points in one dimension's value sequence: `(onset, from, to)`,
/// per the module's rule. Pure, so the rule is testable on its own.
fn change_points(values: &[u32], window: usize, pace: &mut Pace) -> Result<Vec<(usize, u32, u32)>> {
    let walk = dominance_walk(values, window, pace)?;
    let mut points: Vec<(usize, u32, u32)> = Vec::new();
    let mut floor = 0;
    for &switch in &walk.switches {
        // A confirmation reads at most a window either side.
        pace.add(2 * window)?;
        if confirmed(values, switch, window, floor) {
            points.push((switch.0, switch.1, switch.2));
            floor = switch.0;
        }
    }
    // A fresh value at the very end: every run since it appeared carries it,
    // it was rare before, but there are not yet enough of its runs to fill
    // half a window. A deploy an hour ago is exactly this, and it is the
    // change an operator asks about.
    if let (Some(incumbent), Some(&last)) = (walk.current, values.last()) {
        let tail = values.iter().rev().take_while(|v| **v == last).count();
        pace.add(tail + window)?;
        let onset = values.len() - tail;
        let before = &values[onset.saturating_sub(window)..onset];
        if last != incumbent
            && tail >= MIN_ESTABLISHED_RUNS
            && (walk.current_runs >= MIN_ESTABLISHED_RUNS || !walk.switches.is_empty())
            && share(before, last) <= CONFIRM_RARE_SHARE
        {
            points.push((onset, incumbent, last));
        }
    }
    Ok(points)
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

/// Jensen–Shannon distance between adjacent windows of `paths` at every
/// split, by sliding counts: `(split, distance)`.
fn split_scores(
    paths: &[u32],
    window: usize,
    deadline: Option<Deadline>,
) -> Result<Vec<(usize, f64)>> {
    let n = paths.len();
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
        if (split - window) % 512 == 0 {
            Deadline::check(deadline, 0)?;
        }
    }
    Ok(scores)
}

/// Behaviour change points over a cohort's path sequence, per the module's
/// rule — by **sample splitting**, because choosing the strongest of
/// thousands of splits and then testing that same split as if it had been
/// fixed in advance would call noise significant most of the time.
///
/// The even-indexed runs choose: each run of splits whose distance between
/// adjacent windows reaches the threshold contributes its peak; peaks are
/// taken strongest first, at least a window apart, none within a window of
/// a configuration change, at most [`MAX_BEHAVIOR_CANDIDATES`]. The
/// odd-indexed runs, which took no part in choosing, then test each
/// candidate: their windows either side of it must reach the threshold too,
/// and a seeded permutation test of those windows must give a p-value
/// below [`BEHAVIOR_ALPHA`] after Bonferroni's correction for the number of
/// candidates tested. Under the independence the module assumes, that holds
/// the chance of reporting any behaviour change in a cohort whose paths
/// never changed at 1%.
fn behavior_points(
    paths: &[u32],
    config_onsets: &[usize],
    threshold: f64,
    seed: u64,
    deadline: Option<Deadline>,
) -> Result<Vec<BehaviorPoint>> {
    let even: Vec<u32> = paths.iter().step_by(2).copied().collect();
    let odd: Vec<u32> = paths.iter().skip(1).step_by(2).copied().collect();
    let window = (even.len() / 6).clamp(MIN_BEHAVIOR_WINDOW, MAX_BEHAVIOR_WINDOW);
    if even.len() < 2 * window {
        return Ok(Vec::new());
    }
    let scores = split_scores(&even, window, deadline)?;
    let mut peaks: Vec<(usize, f64)> = Vec::new();
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
        peaks.push(best);
    }
    peaks.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    // Candidates, fixed before any test runs (which is what lets the
    // correction count them): full-sequence onset, and the number of odd
    // runs before it.
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for (split, _) in peaks {
        if candidates.len() == MAX_BEHAVIOR_CANDIDATES {
            break;
        }
        let onset = 2 * split;
        if kept.iter().any(|other| other.abs_diff(split) < window)
            || config_onsets
                .iter()
                .any(|config| config.abs_diff(onset) < 2 * window)
        {
            continue;
        }
        kept.push(split);
        // Odd run j sits at 2j + 1, so `onset / 2` of them come before it.
        let odd_before = onset / 2;
        if odd_before >= window && odd_before + window <= odd.len() {
            candidates.push((onset, odd_before));
        }
    }
    let tested = candidates.len();
    let mut accepted = Vec::new();
    for (onset, odd_before) in candidates {
        Deadline::check(deadline, 0)?;
        let left = &odd[odd_before - window..odd_before];
        let right = &odd[odd_before..odd_before + window];
        let observed = stats::js_of(left, right).unwrap_or(0.0);
        if observed < threshold {
            continue;
        }
        let give_up = BEHAVIOR_ALPHA / tested as f64;
        let p_value = stats::js_permutation_test(
            left,
            right,
            observed,
            BEHAVIOR_PERMUTATIONS,
            seed ^ (onset as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
            Some(give_up),
        );
        let adjusted = (p_value * tested as f64).min(1.0);
        if adjusted < BEHAVIOR_ALPHA {
            accepted.push(BehaviorPoint {
                onset: locate(paths, onset, 2 * window),
                shift: Shift {
                    js_distance: observed,
                    p_value: adjusted,
                },
            });
        }
    }
    Ok(accepted)
}

/// Where exactly a behaviour change chosen at even run `onset` begins: the
/// odd run just before it may already be on the new side. Of the two splits,
/// the one whose adjacent windows (`width` runs each) differ more; the even
/// split on a tie.
fn locate(paths: &[u32], onset: usize, width: usize) -> usize {
    let distance = |split: usize| {
        let left = &paths[split.saturating_sub(width)..split];
        let right = &paths[split..(split + width).min(paths.len())];
        stats::js_of(left, right).unwrap_or(0.0)
    };
    if onset >= 2 && distance(onset - 1) > distance(onset) {
        onset - 1
    } else {
        onset
    }
}

/// Where a change's verdict basis ranks it: quality first.
fn basis_rank(change: &Change) -> u8 {
    match change.decided_by {
        Some(Basis::Quality) => 0,
        Some(Basis::Efficiency) => 1,
        None => 2,
    }
}

/// The ranking of the module docs: regressions first, then improvements,
/// neutral and insufficient data; within each, changes a quality metric
/// decided before those an efficiency metric decided; then larger effects,
/// then newer, then by dimension.
fn rank(a: &Change, b: &Change) -> std::cmp::Ordering {
    a.verdict
        .cmp(&b.verdict)
        .then(basis_rank(a).cmp(&basis_rank(b)))
        .then(
            b.effect_size
                .partial_cmp(&a.effect_size)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then(b.at_ns.cmp(&a.at_ns))
        .then(a.dimension.cmp(&b.dimension))
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
    let mut pace = Pace::new(deadline);
    for dimension in Dimension::CONFIG {
        let index = dimension.index();
        // The sequence, the volatility count and the current value: three
        // reads of the cohort's runs.
        pace.add(3 * runs.len())?;
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
        let points = change_points(&values, window, &mut pace)?;
        if points.len() > MAX_CHANGES_PER_DIMENSION {
            // A configuration that changes this often is a per-run value
            // with a slower rhythm, not a history of deploys.
            volatile.push(dimension);
            continue;
        }
        for (onset, from, to) in points {
            raw.push(RawChange {
                dimension,
                onset: sequence[onset].0,
                from,
                to,
                shift: None,
            });
        }
    }
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
    raw.sort_by(|a, b| a.onset.cmp(&b.onset).then(a.dimension.cmp(&b.dimension)));
    let onsets: Vec<usize> = raw.iter().map(|change| change.onset).collect();

    let mut built: Vec<(Change, Vec<u32>, Vec<u32>)> = Vec::with_capacity(raw.len());
    for position in 0..raw.len() {
        Deadline::check(deadline, 0)?;
        built.push(build_change(position, &raw, &onsets, runs, context));
    }
    built.sort_by(|a, b| rank(&a.0, &b.0));
    let truncated = built.len() > MAX_CHANGES_PER_AGENT;
    built.truncate(MAX_CHANGES_PER_AGENT);
    // The path shift across each reported configuration change: a fixed
    // split (the change's own), so its permutation test needs no correction
    // — and it is run only for the changes reported.
    let mut changes = Vec::with_capacity(built.len());
    for (mut change, left, right) in built {
        if change.behavior_shift.is_none()
            && left.len() >= MIN_RUNS_PER_SIDE
            && right.len() >= MIN_RUNS_PER_SIDE
        {
            Deadline::check(deadline, 0)?;
            let distance = stats::js_of(&left, &right).unwrap_or(0.0);
            let p_value = stats::js_permutation_p(
                &left,
                &right,
                distance,
                SHIFT_PERMUTATIONS,
                seed ^ change.at_ns.rotate_left(17) ^ change.dimension.index() as u64,
            );
            change.behavior_shift = Some(Shift {
                js_distance: distance,
                p_value,
            });
        }
        changes.push(change);
    }
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
        unnamed: false,
    })
}

/// One change point with its sides, deltas, verdict and detail — and the
/// two sides' paths (nearest the change, at most [`SHIFT_SIDE_RUNS`] each),
/// for the shift test run on the changes that are reported. `raw` is sorted
/// by onset, and `onsets` is its onsets.
fn build_change(
    position: usize,
    raw: &[RawChange],
    onsets: &[usize],
    runs: &[&RunView],
    context: &Context<'_>,
) -> (Change, Vec<u32>, Vec<u32>) {
    let change = &raw[position];
    let onset = change.onset;
    // Co-changes are neighbours in onset order.
    let low = onsets.partition_point(|&other| other + CO_CHANGE_RUNS < onset);
    let high = onsets.partition_point(|&other| other <= onset + CO_CHANGE_RUNS);
    let mut co_changes: Vec<Dimension> = raw[low..high]
        .iter()
        .enumerate()
        .filter(|(offset, candidate)| {
            low + offset != position && candidate.dimension != change.dimension
        })
        .map(|(_, candidate)| candidate.dimension)
        .collect();
    co_changes.sort();
    co_changes.dedup();
    // Sides stop at the neighbouring change points, co-changes excepted.
    let previous = if low > 0 { onsets[low - 1] } else { 0 };
    let next = onsets.get(high).copied().unwrap_or(runs.len());
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
    let (verdict, decided_by, effect_size, explanation) = verdict(&before, &after, &tests, &deltas);
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

    let (from, to, detail) = match change.dimension {
        Dimension::Behavior => {
            let (from, to, rows) = path_detail(&before, &after, context);
            (from, to, Some(json!({ "paths": rows })))
        }
        dimension => {
            let (before, after) = context.changed_samples(dimension, change.from, change.to);
            (
                context.display(dimension, change.from),
                context.display(dimension, change.to),
                value_detail(before, after),
            )
        }
    };
    let change = Change {
        at_ns: runs[onset].start_ns,
        dimension: change.dimension,
        from,
        to,
        runs_before: before.len(),
        runs_after: after.len(),
        deltas,
        verdict,
        decided_by,
        effect_size,
        explanation,
        co_changes,
        behavior_shift: change.shift,
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
    };
    (change, left, right)
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
    if let Some(test) = stats::proportions(e1, before.len(), e2, after.len()) {
        error_rate.p_value = Some(test.p_value);
        let (b, a) = (
            error_rate.before.unwrap_or(0.0),
            error_rate.after.unwrap_or(0.0),
        );
        if judged(before.len(), after.len()) {
            tests.push(MetricTest {
                p_value: test.p_value,
                worse: a > b,
                effect: stats::cohens_h(b, a),
                quality: true,
                phrase: format!("error rate {} → {}", percent(b), percent(a)),
            });
        }
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
    if let Some(test) = stats::proportions(s1, d1, s2, d2) {
        success_rate.p_value = Some(test.p_value);
        let (b, a) = (
            success_rate.before.unwrap_or(0.0),
            success_rate.after.unwrap_or(0.0),
        );
        // Only the runs that ended are rated: sixty runs a side can hold one
        // that ended, and one is no evidence about a rate.
        if judged(d1, d2) {
            tests.push(MetricTest {
                p_value: test.p_value,
                worse: a < b,
                effect: stats::cohens_h(b, a),
                quality: true,
                phrase: format!("success rate {} → {}", percent(b), percent(a)),
            });
        }
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
        // The test's own samples decide eligibility: cost is tested over
        // priced runs only, which can be far fewer than the side.
        if judged(log_before.len(), log_after.len()) {
            tests.push(MetricTest {
                p_value: test.p_value,
                worse: test.statistic > 0.0,
                effect: stats::cohens_d(log_before, log_after),
                quality: false,
                phrase: phrase(b.unwrap_or(0.0), a.unwrap_or(0.0)),
            });
        }
    }
    delta
}

/// Whether a metric measured on `before` and `after` observations takes part
/// in the verdict: at least [`MIN_RUNS_PER_SIDE`] of them on each side — the
/// same bar the verdict sets on the sides themselves, applied to the
/// observations the metric's test actually used. A metric short of it is
/// still reported, p-value and all, for a reader to weigh.
fn judged(before: usize, after: usize) -> bool {
    before >= MIN_RUNS_PER_SIDE && after >= MIN_RUNS_PER_SIDE
}

fn tool_mix(before: &[&RunView], after: &[&RunView], context: &Context<'_>) -> Vec<ToolShare> {
    let totals = |side: &[&RunView]| {
        let mut counts: HashMap<u64, u64> = HashMap::new();
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
    let share = |counts: &HashMap<u64, u64>, total: u64, tool: u64| {
        if total == 0 {
            0.0
        } else {
            counts.get(&tool).copied().unwrap_or(0) as f64 / total as f64
        }
    };
    let mut tools: Vec<u64> = b.keys().chain(a.keys()).copied().collect();
    tools.sort_unstable();
    tools.dedup();
    let mut rows: Vec<ToolShare> = tools
        .into_iter()
        .map(|tool| ToolShare {
            tool: context.tool_name(tool),
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

/// The verdict, the kind of metric that decided it, its effect size and its
/// sentence.
fn verdict(
    before: &[&RunView],
    after: &[&RunView],
    tests: &[MetricTest],
    deltas: &Deltas,
) -> (Verdict, Option<Basis>, f64, String) {
    if before.len() < MIN_RUNS_PER_SIDE || after.len() < MIN_RUNS_PER_SIDE {
        return (
            Verdict::InsufficientData,
            None,
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
    type Decision<'t> = (
        Verdict,
        Option<Basis>,
        Vec<&'t MetricTest>,
        Vec<&'t MetricTest>,
    );
    let (verdict, decided_by, deciding, others): Decision<'_> = if !quality_worse.is_empty() {
        let mut others = quality_better.clone();
        others.extend(efficiency_better.iter().copied());
        (
            Verdict::Regression,
            Some(Basis::Quality),
            quality_worse,
            others,
        )
    } else if !quality_better.is_empty() {
        (
            Verdict::Improvement,
            Some(Basis::Quality),
            quality_better,
            efficiency_worse.clone(),
        )
    } else if !efficiency_worse.is_empty() {
        (
            Verdict::Regression,
            Some(Basis::Efficiency),
            efficiency_worse,
            efficiency_better.clone(),
        )
    } else if !efficiency_better.is_empty() {
        (
            Verdict::Improvement,
            Some(Basis::Efficiency),
            efficiency_better,
            Vec::new(),
        )
    } else {
        (Verdict::Neutral, None, Vec::new(), Vec::new())
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
    (verdict, decided_by, effect, explanation)
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
/// the window's agent runs, `400` when `agent` or `environment` excludes it
/// or two tenants share its id), and `tenant` (an unbound caller's scope; absent
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
        // A canary carries the new value, but ran before the change: it is
        // downstream of nothing. A straggler after it is not downstream
        // either: it still carries the old value.
        for run in ["run-030", "run-065"] {
            let placed = ChangeQuery {
                run: Some(run.to_owned()),
                ..query()
            };
            let context = store
                .changes(None, &placed, NOW)
                .expect("analyses")
                .run
                .expect("placed");
            assert!(context.downstream_of.is_empty(), "{run}: {context:#?}");
        }
    }

    #[test]
    fn a_multi_agent_prompt_change_diffs_the_prompt_that_changed() {
        // Every run has a planner call and an executor call with different
        // system prompts; only the executor's changes.
        let fixture = TempStore::open("radar-multi", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..120u64 {
            let mut run = RunSpec {
                tenant: "",
                id: format!("run-{index:03}"),
                start: BASE + index * MINUTE,
                prompt: "You plan the work.",
                version: "1.0",
                tools: &["search"],
                failed: false,
                environment: None,
            }
            .spans();
            let executor = if index < 60 {
                "You execute. Use the search tool."
            } else {
                "You execute. Use the browse tool."
            };
            if let Some(last) = run.last_mut() {
                last.attributes.insert(
                    "gen_ai.input.messages".to_owned(),
                    json!(json!([{"role": "system", "content": executor}]).to_string()),
                );
            }
            spans.extend(run);
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let prompt = find(&report, Dimension::SystemPrompt).expect("found");
        assert!(
            prompt.from.contains(" + ") && prompt.to.contains(" + "),
            "a set"
        );
        let diff = &prompt
            .detail
            .as_ref()
            .expect("the changed member is diffed")["diff"];
        assert_eq!(diff["before"], "search");
        assert_eq!(diff["after"], "browse");
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
    fn a_run_whose_root_is_read_long_before_its_step_is_read_whole() {
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
        // Ten agent runs and a cap of ten: the thirty request traces are no
        // agent runs and cost the cap nothing, and each run's wrapper, read
        // long before its model call, still starts it.
        assert!(!report.truncated, "{report:#?}");
        assert_eq!(report.covered_since_ns, None);
        assert_eq!(report.runs_scanned, 10);
        assert_eq!(
            report.agents[0].first_run_ns, BASE,
            "the wrapper starts the run"
        );
        // One fewer than the runs: exactly the oldest is let go, by its
        // model call's start.
        let tight = ChangeQuery {
            max_runs: 9,
            ..query()
        };
        let report = store.changes(None, &tight, NOW).expect("analyses");
        assert!(report.truncated);
        assert_eq!(report.covered_since_ns, Some(BASE + 101));
        assert_eq!(report.runs_scanned, 9);
        assert_eq!(report.agents[0].first_run_ns, BASE + MINUTE);
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

    /// xorshift64*, for seeded synthetic sequences.
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        /// An index drawn from `weights` (which sum to 1).
        fn pick(&mut self, weights: &[f64]) -> u32 {
            let draw = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
            let mut total = 0.0;
            for (index, weight) in weights.iter().enumerate() {
                total += weight;
                if draw < total {
                    return index as u32;
                }
            }
            weights.len() as u32 - 1
        }
    }

    /// One run of one llm step (with `model`) and `tools` tool steps, every
    /// span 15 ns long, for the agent `service`.
    fn simple_run(
        service: &str,
        id: &str,
        start: u64,
        model: &str,
        prompt: &str,
        tools: usize,
        failed: bool,
    ) -> Vec<Span> {
        let span = |span_id: String, offset: u64, attributes: Value| Span {
            trace_id: format!("trace-{id}"),
            span_id,
            tenant: String::new(),
            parent_span_id: None,
            name: "step".to_owned(),
            start_time_ns: start + offset,
            end_time_ns: start + offset + 15,
            status: "ok".to_owned(),
            service: service.to_owned(),
            attributes: attributes.as_object().cloned().unwrap_or_default(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        };
        let mut spans = vec![span(
            "a-llm".to_owned(),
            0,
            json!({
                "session.id": id,
                "gen_ai.operation.name": "chat",
                "gen_ai.request.model": model,
                "gen_ai.response.model": model,
                "gen_ai.input.messages": json!([
                    {"role": "system", "content": prompt},
                    {"role": "user", "content": "hi"}
                ]).to_string(),
            }),
        )];
        for index in 0..tools {
            spans.push(span(
                format!("b-tool-{index}"),
                0,
                json!({"session.id": id, "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": "lookup"}),
            ));
        }
        if failed {
            spans[0].status = "error".to_owned();
        }
        spans
    }

    #[test]
    fn a_stable_model_split_with_constant_runs_reports_nothing() {
        // A router sends each run to one of two models at random, forever;
        // every run is exactly two steps and 15 ns long. Nothing changed, so
        // nothing is a change, and constant metrics are no regression.
        let fixture = TempStore::open("radar-stable-split", crate::Config::default());
        let store = fixture.store();
        let mut seeded = Seeded(0x5eed_0001);
        let mut spans = Vec::new();
        for index in 0..800u64 {
            let model = ["model-a", "model-b"][seeded.pick(&[0.5, 0.5]) as usize];
            spans.extend(simple_run(
                "router-bot",
                &format!("run-{index:04}"),
                BASE + index * MINUTE,
                model,
                "Route the request.",
                1,
                false,
            ));
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let agent = &report.agents[0];
        assert_eq!(agent.runs, 800);
        assert!(agent.changes.is_empty(), "{:#?}", agent.changes);
    }

    #[test]
    fn a_quality_regression_outranks_a_one_step_efficiency_change() {
        // 1.0 → 1.1 took the error rate from 5% to 40%; 1.1 → 1.2 added one
        // tool call to every run and nothing else. The quality regression is
        // the headline.
        let fixture = TempStore::open("radar-rank", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..180u64 {
            let (version, tools, failed): (&str, &[&str], bool) = match index {
                0..=59 => ("1.0", &["search"], index % 20 == 0),
                60..=119 => ("1.1", &["search"], index % 5 < 2),
                _ => ("1.2", &["search", "search"], index % 5 < 2),
            };
            spans.extend(
                RunSpec {
                    tenant: "",
                    id: format!("run-{index:03}"),
                    start: BASE + index * MINUTE,
                    prompt: "Prompt.",
                    version,
                    tools,
                    failed,
                    environment: None,
                }
                .spans(),
            );
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let agent = &report.agents[0];
        let top = &agent.changes[0];
        assert_eq!(top.dimension, Dimension::ServiceVersion, "{top:#?}");
        assert_eq!((top.from.as_str(), top.to.as_str()), ("1.0", "1.1"));
        assert_eq!(top.verdict, Verdict::Regression);
        let efficiency = agent
            .changes
            .iter()
            .find(|change| change.to == "1.2")
            .expect("the extra step is a change too");
        assert_eq!(efficiency.verdict, Verdict::Regression, "{efficiency:#?}");
        assert!(efficiency.effect_size <= stats::MAX_COHENS_D);
        assert!(
            agent.summary.contains("(1.0 → 1.1)"),
            "the headline is the quality regression: {}",
            agent.summary
        );
    }

    #[test]
    fn agent_runs_behind_workflow_roots_survive_the_run_cap() {
        // Every run's first span is an OpenLLMetry workflow root (not a
        // step), sealed and read before any of the runs' model calls.
        let fixture = TempStore::open("radar-roots", crate::Config::default());
        let store = fixture.store();
        let roots: Vec<Span> = (0..200u64)
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
        store.ingest_batch(roots).expect("ingests");
        store.flush().expect("seals");
        let steps: Vec<Span> = (0..200u64)
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
            max_runs: 100,
            ..query()
        };
        let report = store.changes(None, &capped, NOW).expect("analyses");
        assert!(report.truncated);
        assert_eq!(report.runs_scanned, 100, "the newest hundred, whole");
        assert_eq!(report.covered_since_ns, Some(BASE + 99 * MINUTE + 101));
        assert_eq!(
            report.agents[0].first_run_ns,
            BASE + 100 * MINUTE,
            "each run starts at its root"
        );
    }

    #[test]
    fn a_noisy_agent_does_not_hide_another_agents_prompt_edit() {
        // noisy-bot's prompt differs on every run (too varied to search);
        // it is read first and has more distinct prompts than one table of
        // names holds. support-bot's prompt edit must still be found, diffed.
        let fixture = TempStore::open("radar-noisy", crate::Config::default());
        let store = fixture.store();
        let word = |mut index: u64| {
            let mut text = String::new();
            for _ in 0..4 {
                text.push((b'a' + (index % 26) as u8) as char);
                index /= 26;
            }
            text
        };
        let noisy: Vec<Span> = (0..2_100u64)
            .flat_map(|index| {
                simple_run(
                    "noisy-bot",
                    &format!("noisy-{index:04}"),
                    BASE + index * 1_000_000,
                    "m",
                    &format!("Answer about {}.", word(index)),
                    0,
                    false,
                )
            })
            .collect();
        store.ingest_batch(noisy).expect("ingests");
        store.flush().expect("seals");
        two_eras(
            store,
            "",
            ("Always escalate refunds.", "1.0", &["search"]),
            ("Never escalate refunds.", "1.0", &["search"]),
            |index| index % 20 == 0,
            |index| index % 5 < 2,
        );
        let report = store.changes(None, &query(), NOW).expect("analyses");
        let noisy = report
            .agents
            .iter()
            .find(|agent| agent.agent == "noisy-bot")
            .expect("noisy-bot");
        assert_eq!(noisy.volatile, [Dimension::SystemPrompt]);
        let support = report
            .agents
            .iter()
            .find(|agent| agent.agent == "support-bot")
            .expect("support-bot");
        let prompt = support
            .changes
            .iter()
            .find(|change| change.dimension == Dimension::SystemPrompt)
            .expect("the edit is found");
        assert_eq!(prompt.verdict, Verdict::Regression);
        let diff = &prompt.detail.as_ref().expect("diffed")["diff"];
        assert_eq!(diff["before"], "Always");
        assert_eq!(diff["after"], "Never");
        assert!(
            !support.unnamed,
            "support-bot keeps its names: {support:#?}"
        );
    }

    #[test]
    fn stationary_paths_are_not_a_behaviour_change() {
        // Runs drawn independently from one path distribution, forever: any
        // behaviour change found here is a false alarm. The rate is held at
        // 1% per cohort, so at most one of these may alarm.
        let mut seeded = Seeded(0x5eed_0002);
        let skewed = [0.4, 0.2, 0.1, 0.1, 0.05, 0.05, 0.05, 0.05];
        let even = [0.05; 20];
        let mut alarms = 0;
        for (cohorts, runs, weights) in [(12, 600, &skewed[..]), (4, 2_000, &even[..])] {
            for cohort in 0..cohorts {
                let paths: Vec<u32> = (0..runs).map(|_| seeded.pick(weights)).collect();
                let found =
                    behavior_points(&paths, &[], DEFAULT_BEHAVIOR_THRESHOLD, cohort as u64, None)
                        .expect("no deadline");
                alarms += usize::from(!found.is_empty());
            }
        }
        assert!(alarms <= 1, "{alarms} of 16 stationary cohorts alarmed");
    }

    #[test]
    fn random_mixtures_are_not_change_points() {
        // An A/B split, a 70/30 split and a three-way split, drawn at random
        // per run, forever: none is a configuration change.
        let mut seeded = Seeded(0x5eed_0003);
        let mut alarmed = Vec::new();
        for (sequences, runs, weights) in [
            (5, 1_000, &[0.5, 0.5][..]),
            (5, 1_000, &[0.7, 0.3][..]),
            (3, 2_000, &[0.34, 0.33, 0.33][..]),
            (3, 5_000, &[0.5, 0.5][..]),
        ] {
            for _ in 0..sequences {
                let values: Vec<u32> = (0..runs).map(|_| seeded.pick(weights)).collect();
                let points = found(&values, detection_window(runs));
                if !points.is_empty() {
                    alarmed.push((runs, points));
                }
            }
        }
        assert!(alarmed.len() <= 1, "{alarmed:?}");
    }

    #[test]
    fn an_environment_past_the_256th_is_still_filterable() {
        let fixture = TempStore::open("radar-environments", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        for index in 0..300u64 {
            spans.extend(
                RunSpec {
                    tenant: "",
                    id: format!("run-{index:03}"),
                    start: BASE + index * MINUTE,
                    prompt: "Prompt.",
                    version: "1.0",
                    tools: &["search"],
                    failed: false,
                    environment: Some(&format!("pr-{index:03}")),
                }
                .spans(),
            );
        }
        store.ingest_batch(spans).expect("ingests");
        let preview = ChangeQuery {
            environment: Some("pr-299".to_owned()),
            ..query()
        };
        let report = store.changes(None, &preview, NOW).expect("analyses");
        assert_eq!(report.agents.len(), 1, "{report:#?}");
        assert_eq!(report.agents[0].runs, 1);
    }

    #[test]
    fn runs_cut_off_by_a_past_until_are_not_compared_as_finished() {
        let fixture = TempStore::open("radar-until", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "2.0", &["search"]),
            |_| false,
            |_| false,
        );
        // The window ends just after the last run's spans, a month before
        // now: the runs ending within the idle window before `until` may
        // have gone on after it.
        let cut = ChangeQuery {
            until_ns: BASE + 119 * MINUTE + 600,
            ..query()
        };
        let report = store.changes(None, &cut, NOW).expect("analyses");
        let version = find(&report, Dimension::ServiceVersion).expect("found");
        assert_eq!(version.runs_before, 60);
        assert!(version.runs_after < 60, "{}", version.runs_after);
        assert!(version.runs_after >= 40, "{}", version.runs_after);
    }

    #[test]
    fn a_named_run_its_filters_exclude_is_refused_by_name() {
        let fixture = TempStore::open("radar-run-filtered", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Prompt.", "1.0", &["search"]),
            ("Prompt.", "2.0", &["search"]),
            |_| false,
            |_| false,
        );
        let excluded = ChangeQuery {
            run: Some("-run-061".to_owned()),
            agent: Some("billing-bot".to_owned()),
            ..query()
        };
        match store.changes(None, &excluded, NOW) {
            Err(Error::Invalid(reason)) => {
                assert!(reason.contains("agent filter"), "{reason}");
                // The refusal names only what the caller sent: an error is
                // not framed as telemetry, so stored text stays out of it.
                assert!(!reason.contains("support-bot"), "{reason}");
            }
            other => panic!("expected a refusal naming the filter, got {other:?}"),
        }
        let reply = serve(
            store,
            "GET",
            ROUTE,
            &format!("run=-run-061&environment=prod&since={}", BASE - MINUTE),
            None,
            NOW,
        );
        assert_eq!(reply.status, 400, "{:?}", reply.body);
        assert!(
            reply.body["error"]
                .as_str()
                .is_some_and(|error| error.contains("environment")),
            "{:?}",
            reply.body
        );
    }

    #[test]
    fn the_first_pass_remembers_no_run_without_a_step_and_at_most_twice_the_cap() {
        let mut selection = Selection::new(10);
        for index in 0..20_000u64 {
            selection.visit(&plain_span(
                &format!("plain-{index}"),
                BASE + index,
                json!({}),
            ));
        }
        assert!(selection.firsts.is_empty(), "request traces cost nothing");
        for index in 0..100u64 {
            selection.visit(&plain_span(
                &format!("agent-{index}"),
                BASE + index * MINUTE,
                json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m"}),
            ));
            assert!(selection.firsts.len() <= 2 * 10 + 1);
        }
        let (held, mark) = selection.finish();
        assert_eq!(held[""].len(), 10);
        assert_eq!(mark, Some(BASE + 89 * MINUTE));
        assert!(held[""].contains("trace:agent-99") && !held[""].contains("trace:agent-89"));
        // A held run whose earlier step turns up at or before the mark
        // started there: it is let go, not shortened.
        let step = |trace: &str, start: u64| {
            plain_span(
                trace,
                start,
                json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m"}),
            )
        };
        let mut selection = Selection::new(1);
        for (trace, start) in [("a", 10), ("b", 20), ("c", 30)] {
            selection.visit(&step(trace, start));
        }
        assert_eq!(selection.watermark, Some(20));
        assert_eq!(selection.firsts.len(), 1, "c is held");
        let mut early = step("c", 5);
        early.span_id = "c-early".to_owned();
        selection.visit(&early);
        let (held, mark) = selection.finish();
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(mark, Some(20));
    }

    #[test]
    fn what_a_run_holds_is_clipped_whatever_its_spans_carry() {
        // Every string a span could plant in an accumulator, 100 KB long.
        let huge = "x".repeat(100_000);
        let mut span = plain_span(
            &huge,
            BASE,
            json!({
                "session.id": "r",
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.name": huge,
                "gen_ai.agent.name": huge,
                semconv::OUTCOME_KEYS[0]: huge,
                "service.version": huge,
                "deployment.environment.name": huge,
            }),
        );
        span.span_id.clone_from(&huge);
        span.service.clone_from(&huge);
        let mut tables = Tables::new();
        let mut acc = RunAcc::default();
        acc.absorb(&span, &semconv::facts(&span.attributes), &mut tables, 0);
        assert_eq!(acc.steps, 1);
        for key in [&acc.earliest, &acc.first_step, &acc.last] {
            let (_, trace, id) = key.as_ref().expect("kept");
            assert!(trace.len() <= MAX_ID_BYTES && id.len() <= MAX_ID_BYTES);
        }
        assert!(acc.earliest_service.len() <= MAX_NAME_BYTES);
        assert!(acc
            .first_step_agent
            .as_ref()
            .is_some_and(|agent| agent.len() <= MAX_NAME_BYTES));
        let (_, declared) = acc.declared.as_ref().expect("declared");
        assert!(declared.len() <= MAX_OUTCOME_BYTES);
        for table in [&tables.tokens, &tables.tools] {
            assert!(table
                .names
                .values()
                .all(|name| name.len() <= MAX_NAME_BYTES));
        }
        let versions = &tables.values[Dimension::ServiceVersion.index()];
        assert!(versions
            .names
            .values()
            .all(|name| name.chars().count() <= fingerprint::MAX_VALUE_CHARS + 1));
        // A long run id is held as a bounded key that still tells runs apart.
        let one = run_key(format!("{huge}-1"));
        let two = run_key(format!("{huge}-2"));
        assert!(one.len() <= MAX_RUN_ID_BYTES && two.len() <= MAX_RUN_ID_BYTES);
        assert_ne!(one, two);
        assert_eq!(run_key("short".to_owned()), "short");
    }

    #[test]
    fn the_read_stops_extracting_once_its_deadline_has_passed() {
        let fixture = TempStore::open("radar-read-expiry", crate::Config::default());
        let expired = Some(Deadline::capped(None, std::time::Duration::ZERO));
        let held: HashMap<String, HashSet<String>> =
            [(String::new(), ["r".to_owned()].into_iter().collect())]
                .into_iter()
                .collect();
        let mut fold = Accumulate::new(held, expired);
        for index in 0..200u64 {
            let mut span = plain_span(
                "t",
                BASE + index,
                json!({"session.id": "r", "gen_ai.operation.name": "chat",
                       "gen_ai.request.model": "m"}),
            );
            span.span_id = format!("s{index}");
            fold.visit(fixture.store(), &span);
        }
        assert!(fold.expired);
        let acc = &fold.runs[""].1["r"];
        assert_eq!(
            acc.steps as usize, EXTRACTION_CHECK_INTERVAL,
            "nothing is extracted after the check that found the deadline passed"
        );
    }

    #[test]
    fn the_analysis_honours_the_deadline_after_a_read_that_did() {
        // The read finishes inside its budget; the analysis is handed one
        // already spent, and every stage of it refuses on its own.
        let fixture = TempStore::open("radar-analysis-deadline", crate::Config::default());
        let store = fixture.store();
        two_eras(
            store,
            "",
            ("Use search.", "1.0", &["search"]),
            ("Use browse.", "2.0", &["browse", "browse"]),
            |_| false,
            |index| index % 5 < 2,
        );
        let window = store
            .read_window(None, &query(), NOW, None)
            .expect("reads within budget");
        let spent = Some(Deadline::capped(None, std::time::Duration::ZERO));
        assert!(analyse_window(&window, &query(), None).is_ok());
        assert!(matches!(
            analyse_window(&window, &query(), spent),
            Err(Error::DeadlineExceeded(_))
        ));
        let runs: Vec<&RunView> = window.views.iter().collect();
        let context = Context {
            tables: &window.tables,
            combos: &window.combos,
            paths: &window.paths,
            threshold: DEFAULT_BEHAVIOR_THRESHOLD,
            tenant: 0,
            unnamed: Cell::new(false),
        };
        assert!(matches!(
            analyse("", "support-bot", &runs, &context, spent),
            Err(Error::DeadlineExceeded(_))
        ));
        let paths: Vec<u32> = (0..600).map(|index| (index % 3) as u32).collect();
        assert!(matches!(
            behavior_points(&paths, &[], DEFAULT_BEHAVIOR_THRESHOLD, 1, spent),
            Err(Error::DeadlineExceeded(_))
        ));
    }

    #[test]
    fn change_points_follow_dominance_not_appearance() {
        let ones = |n: usize, value: u32| vec![value; n];
        let concat = |parts: &[Vec<u32>]| parts.concat();
        // A clean switch is placed at its first run.
        assert_eq!(found(&concat(&[ones(30, 0), ones(30, 1)]), 8), [(30, 0, 1)]);
        // Two in a row.
        assert_eq!(
            found(&concat(&[ones(30, 0), ones(30, 1), ones(30, 2)]), 10),
            [(30, 0, 1), (60, 1, 2)]
        );
        // A 5% canary never becomes dominant.
        let canary: Vec<u32> = (0..100).map(|i| u32::from(i % 20 == 0)).collect();
        assert!(found(&canary, 12).is_empty());
        // An even A/B split is not a change either.
        let split: Vec<u32> = (0..100).map(|i| i % 2).collect();
        assert!(found(&split, 12).is_empty());
        // An excursion that comes back within a window is withdrawn.
        let excursion = concat(&[ones(40, 0), ones(6, 1), ones(40, 0)]);
        assert!(
            found(&excursion, 10).is_empty(),
            "{:?}",
            found(&excursion, 10)
        );
        // A first value that never established itself is settling.
        assert_eq!(
            found(&concat(&[ones(1, 5), ones(30, 0), ones(30, 1)]), 8),
            [(31, 0, 1)]
        );
        // A fresh value holding every run at the end is a change before it
        // fills half a window; two runs are not enough to say so.
        assert_eq!(found(&concat(&[ones(60, 0), ones(5, 1)]), 20), [(60, 0, 1)]);
        assert!(found(&concat(&[ones(60, 0), ones(2, 1)]), 20).is_empty());
        assert!(
            found(&concat(&[ones(60, 0), ones(4, 1), ones(1, 0)]), 20).is_empty(),
            "not if the last run went back"
        );
        // Nor when the value was common all along: three in a row at the end
        // of an even split is chance, not a deploy.
        let mut split_tail: Vec<u32> = (0..200).map(|i| i % 2).collect();
        split_tail.extend([0, 1, 1, 1]);
        assert!(
            found(&split_tail, 50).is_empty(),
            "{:?}",
            found(&split_tail, 50)
        );
        // A rollout that ramps up is placed where the new value took over.
        let mut ramp = ones(40, 0);
        ramp.extend((0..20).map(|i| u32::from(i % 4 == 0)));
        ramp.extend(ones(40, 1));
        let points = found(&ramp, 12);
        assert_eq!(points.len(), 1, "{points:?}");
        assert!((40..=62).contains(&points[0].0), "{points:?}");
        // A rollout that held a half-and-half stage for a while is a change
        // when it completes: the old value's share fell from half to none.
        let mut staged = ones(200, 0);
        staged.extend((0..100).map(|i| i % 2));
        staged.extend(ones(200, 1));
        let points = found(&staged, 50);
        assert_eq!(points.len(), 1, "{points:?}");
        assert_eq!((points[0].1, points[0].2), (0, 1));
        assert!((200..=300).contains(&points[0].0), "{points:?}");
        // Once the new value holds, a window's mixture of both is not
        // enough on its own: a stage too short to judge is no completion.
        let mut short = ones(30, 0);
        short.extend((0..10).map(|i| i % 2));
        short.extend(ones(30, 1));
        assert_eq!(found(&short, 8).len(), 1, "a takeover all the same");
        // A mixture that was always there, then a deploy of a third value.
        let mixed: Vec<u32> = (0..300).map(|i| i % 2).collect();
        let mut deployed = mixed.clone();
        deployed.extend(ones(200, 2));
        let points = found(&deployed, 50);
        assert_eq!(points.len(), 1, "{points:?}");
        assert_eq!((points[0].0, points[0].2), (300, 2));
        // A deploy that left more than a fifth of runs on the old value is
        // a mixture, not a change: the radar reports takeovers.
        let mut lingering = ones(200, 0);
        lingering.extend((0..200).map(|i| u32::from(i % 3 != 0)));
        assert!(found(&lingering, 50).is_empty());
    }

    #[test]
    fn top_paths_bucket_repeats_and_mark_continuations() {
        let mut tokens = NameTable::new(16, 16);
        let named = |tokens: &mut NameTable, name: &str| {
            let id = digest(name);
            tokens.record(0, id, 0, name, None);
            id
        };
        let llm = named(&mut tokens, "llm:m");
        let a = named(&mut tokens, "tool:a");
        let b = named(&mut tokens, "tool:b");
        let run = |sequence: &[u64], steps: u32| RunAcc {
            step_keys: sequence
                .iter()
                .enumerate()
                .map(|(index, token)| (index as u64, 0, *token))
                .collect(),
            steps,
            ..RunAcc::default()
        };
        let path = |acc: &RunAcc| top_path(acc, &tokens, 0);
        assert_eq!(
            path(&run(&[llm, a, a, a, llm, b, llm], 7)),
            ("llm:m > tool:a×3-5 > llm:m > tool:b > …".to_owned(), false)
        );
        assert_eq!(
            path(&run(&[llm, a, a, llm], 4)).0,
            "llm:m > tool:a×2 > llm:m"
        );
        assert_eq!(path(&run(&[a; 7], 7)).0, "tool:a×6+");
        assert_eq!(
            path(&run(&[llm, llm], 90)).0,
            "llm:m×2 > …",
            "steps beyond those kept are a continuation"
        );
        assert_eq!(path(&run(&[], 0)).0, "(no steps)");
        // A token without a name is shown by digest, and says so; another
        // tenant's names are not this tenant's.
        let stranger = digest("tool:never-named");
        assert_eq!(
            path(&run(&[llm, stranger], 2)),
            (format!("llm:m > #{stranger:016x}"), true)
        );
        assert!(
            top_path(&run(&[llm], 1), &tokens, 1).1,
            "tenant 1 has no names"
        );
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

    /// One single-step run of `service` on `version`, declaring `outcome`
    /// when given, its one span failing when `failed`.
    fn versioned_run(
        service: &str,
        id: &str,
        start: u64,
        version: &str,
        outcome: Option<&str>,
        failed: bool,
    ) -> Vec<Span> {
        let mut spans = simple_run(service, id, start, "m", "Prompt.", 0, failed);
        for span in &mut spans {
            span.attributes
                .insert("service.version".to_owned(), json!(version));
            if let Some(outcome) = outcome {
                span.attributes
                    .insert(semconv::OUTCOME_KEYS[0].to_owned(), json!(outcome));
            }
        }
        spans
    }

    /// The service-version change of `agent`.
    fn version_change<'r>(report: &'r ChangeReport, agent: &str) -> &'r Change {
        report
            .agents
            .iter()
            .find(|cohort| cohort.agent == agent)
            .and_then(|cohort| {
                cohort
                    .changes
                    .iter()
                    .find(|change| change.dimension == Dimension::ServiceVersion)
            })
            .unwrap_or_else(|| panic!("{agent}'s deploy is found: {report:#?}"))
    }

    #[test]
    fn a_rate_is_judged_on_its_own_runs_and_exactly_when_its_cells_are_sparse() {
        let fixture = TempStore::open("radar-sparse-rates", crate::Config::default());
        let store = fixture.store();
        let mut spans = Vec::new();
        // 60 finished runs a side, but almost none declared an outcome the
        // radar knows: one success before, ten failures after, every other
        // run "escalated" (an outcome, but neither success nor failure).
        for index in 0..120u64 {
            let (version, outcome) = match index {
                0 => ("1.0", "success"),
                1..=59 => ("1.0", "escalated"),
                60..=69 => ("2.0", "failure"),
                _ => ("2.0", "escalated"),
            };
            spans.extend(versioned_run(
                "declaring-bot",
                &format!("declaring-{index:03}"),
                BASE + index * MINUTE,
                version,
                Some(outcome),
                false,
            ));
        }
        // 20 runs a side, no error before and six after: every expected
        // cell count of the table is at least 5 only on one diagonal.
        for index in 0..40u64 {
            spans.extend(versioned_run(
                "erring-bot",
                &format!("erring-{index:03}"),
                BASE + index * MINUTE,
                if index < 20 { "1.0" } else { "2.0" },
                None,
                (20..26).contains(&index),
            ));
        }
        store.ingest_batch(spans).expect("ingests");
        let report = store.changes(None, &query(), NOW).expect("analyses");

        let declaring = version_change(&report, "declaring-bot");
        assert_eq!((declaring.runs_before, declaring.runs_after), (60, 60));
        let success = &declaring.deltas.success_rate;
        assert_eq!((success.n_before, success.n_after), (1, 10));
        // scipy.stats.fisher_exact([[1, 0], [0, 10]]).pvalue == 1/11; the
        // z-test says 0.00091 of the same table.
        let p = success.p_value.expect("reported");
        assert!((p - 1.0 / 11.0).abs() < 1e-9, "{p}");
        assert_eq!(
            declaring.verdict,
            Verdict::Neutral,
            "one run before is no evidence about a success rate: {declaring:#?}"
        );

        let erring = version_change(&report, "erring-bot");
        assert_eq!((erring.runs_before, erring.runs_after), (20, 20));
        // scipy.stats.fisher_exact([[0, 20], [6, 14]]).pvalue; the z-test's
        // 0.0079 would pass Holm's 0.05/4 and call this a regression.
        let p = erring.deltas.error_rate.p_value.expect("reported");
        assert!((p - 0.020_196_020_196_020_2).abs() < 1e-9, "{p}");
        assert_eq!(erring.verdict, Verdict::Neutral, "{erring:#?}");
    }

    /// Change points of `values`, with no deadline.
    fn found(values: &[u32], window: usize) -> Vec<(usize, u32, u32)> {
        change_points(values, window, &mut Pace::new(None)).expect("no deadline")
    }

    /// Blocks of 30 runs on one value, then 30 on the other, alternating:
    /// with a 50-run window every block is an excursion the walk proposes
    /// and then withdraws.
    fn alternating_blocks(runs: usize) -> Vec<u32> {
        (0..runs).map(|index| ((index / 30) % 2) as u32).collect()
    }

    #[test]
    fn the_dominance_walk_is_linear_in_runs() {
        let work = |runs: usize| {
            let mut pace = Pace::new(None);
            change_points(&alternating_blocks(runs), MAX_DETECTION_WINDOW, &mut pace)
                .expect("no deadline");
            pace.work
        };
        let (single, double) = (work(20_000), work(40_000));
        // Twice the runs, about twice the work: rescanning the prefix on
        // every withdrawal made it four times.
        assert!(
            double <= single * 9 / 4,
            "{single} values read for 20k runs, {double} for 40k"
        );
        // And a small multiple of the window per run.
        assert!(
            double <= 2 * MAX_DETECTION_WINDOW * 40_000,
            "{double} values read for 40k runs"
        );
    }

    #[test]
    fn the_dominance_walk_stops_at_the_deadline_not_after_it() {
        let spent = Some(Deadline::capped(None, std::time::Duration::ZERO));
        let mut pace = Pace::new(spent);
        let values = alternating_blocks(200_000);
        assert!(matches!(
            change_points(&values, MAX_DETECTION_WINDOW, &mut pace),
            Err(Error::DeadlineExceeded(_))
        ));
        assert!(
            pace.work <= PACE_CHECK_WORK,
            "refused only after reading {} values",
            pace.work
        );
    }

    /// The dominance walk as first written, which recounted the restored
    /// incumbent over the whole prefix on every withdrawal: the oracle the
    /// linear walk must agree with exactly.
    fn rescanning_walk(values: &[u32], window: usize) -> (Vec<Switch>, Option<u32>, usize) {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        let mut current: Option<u32> = None;
        let mut current_runs = 0usize;
        let mut switches: Vec<Switch> = Vec::new();
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
            if switches.is_empty() && current_runs < MIN_ESTABLISHED_RUNS {
                current = Some(leader);
                current_runs = leader_runs;
                continue;
            }
            if let Some(&(last_onset, last_from, last_to, _)) = switches.last() {
                if last_to == incumbent
                    && last_from == leader
                    && onset.abs_diff(last_onset) < window
                {
                    switches.pop();
                    current = Some(leader);
                    current_runs = values[..=index].iter().filter(|v| **v == leader).count();
                    continue;
                }
            }
            switches.push((onset, incumbent, leader, index));
            current = Some(leader);
            current_runs = leader_runs;
        }
        (switches, current, current_runs)
    }

    #[test]
    fn the_linear_walk_proposes_exactly_what_the_rescanning_walk_did() {
        let mut seeded = Seeded(0x5eed_0004);
        let mut sequences: Vec<(Vec<u32>, usize)> = vec![
            (alternating_blocks(3_000), MAX_DETECTION_WINDOW),
            (alternating_blocks(500), 12),
        ];
        for (runs, weights) in [
            (400, &[0.5, 0.5][..]),
            (1_000, &[0.7, 0.3][..]),
            (2_000, &[0.34, 0.33, 0.33][..]),
            (300, &[0.9, 0.05, 0.05][..]),
        ] {
            for _ in 0..4 {
                let values: Vec<u32> = (0..runs).map(|_| seeded.pick(weights)).collect();
                sequences.push((values, detection_window(runs)));
            }
        }
        // Eras with excursions and returns, drawn at random.
        for _ in 0..8 {
            let mut values = Vec::new();
            while values.len() < 1_500 {
                let value = seeded.pick(&[0.4, 0.3, 0.2, 0.1]);
                let length = 1 + (seeded.next() % 80) as usize;
                values.extend(std::iter::repeat(value).take(length));
            }
            for window in [MIN_DETECTION_WINDOW, 20, MAX_DETECTION_WINDOW] {
                sequences.push((values.clone(), window));
            }
        }
        for (values, window) in sequences {
            let walk = dominance_walk(&values, window, &mut Pace::new(None)).expect("no deadline");
            assert_eq!(
                (walk.switches, walk.current, walk.current_runs),
                rescanning_walk(&values, window),
                "window {window}"
            );
        }
    }
}
