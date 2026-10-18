//! Foresight: the shape of an agent's behaviour, learned from every run it
//! ever made, and a live run's fate predicted from the path it has taken.
//!
//! Traza holds every step of every run. Read together, those runs say
//! something no single trace can: which paths an agent takes, how often each
//! one ends well, and where the paths that end badly part from the ones
//! that do not. This module turns that history into a **Fate Map** — a prefix
//! tree of behaviour per agent — and reads a run against it: "runs that took
//! this path succeeded 3 of 112 times". Nothing here calls or embeds a model.
//! It is counting, done carefully.
//!
//! # Behaviour signatures
//!
//! A run's steps (LLM, tool and agent spans — [`semconv::StepKind::is_step`])
//! in step order map to tokens `kind:name`: the step signature
//! ([`semconv::StepFacts::signature`]), with no request hash, so two calls
//! of the same tool are the same behaviour whatever their arguments.
//! Consecutive repeats of one signature fold into ONE token by bucket —
//! `x`, `x×2`, `x×3-5`, `x×6+` — so a loop is a shape, not an explosion of
//! the tree. A path holds at most [`MAX_DEPTH`] tokens; a deeper run ends in
//! the terminal [`FOLD_TOKEN`] node.
//!
//! # Cohorts
//!
//! Runs are compared with runs of the same agent in the same tenant. The
//! agent is the first step's agent name ([`semconv::StepFacts::agent`]),
//! else the service of the run's earliest span. A run may also declare its
//! task (`traza.task`, else `session.goal`); the value is hashed to a 16-hex
//! key and the map can be narrowed to one task (`?task=`).
//!
//! # The map, and the statistics it is held to
//!
//! A cohort's completed runs — outcome from
//! [`attribution::session_outcome`], so success, failure or abandoned — are
//! folded into the tree; runs still active, or whose outcome cannot be told,
//! are excluded and counted. Each node carries its visits and outcome counts,
//! summed cost and duration, and up to three example runs (the newest of
//! each outcome, a ready-made pair for `/v1/diff`). From those, per node:
//! the success rate with a **95% Wilson interval**, the failure rate, the
//! lift over the cohort's base rate, and the **entropy** of what the agent
//! did next (ending the run counts as one of the choices). A node is a
//! **hotspot** when that entropy is at least one bit with support, and a
//! child is a **fatal fork** when its success rate is at least 25 points
//! below its siblings' pooled rate, both have support, and a two-sided
//! Fisher exact test puts the difference at p < 0.05 — so three unlucky runs
//! are not a fatal fork. Each map also reports its own **calibration**:
//! cross-validated predictions at several depths scored against what
//! happened (Brier score and skill over the base rate), so a reader can see
//! how much the map's foresight is worth before trusting it.
//!
//! # Foresight for one run
//!
//! A run's path is walked down its cohort's map. The run itself is taken out
//! of the statistics it is judged by (leave-one-out), and the verdict is:
//! `novel` when the run took a step no earlier run took at that point;
//! `insufficient_history` when fewer than `min_support` runs share its path;
//! otherwise `likely_failure` when the interval's upper bound is below 50%,
//! `at_risk` when it is below the cohort's base rate, and `on_track`.
//!
//! # Predictive fuses
//!
//! A fuse policy may set `min_success_odds`: on the pre-call check and on
//! verdict queries — never on ingest — a run whose foresight has the support
//! and an upper bound below the odds is given the policy's action. That path
//! reads only a CACHED map; a cold cache answers `insufficient_history`, and
//! the map is built off the request path by [`Store::maintain_fate`].
//!
//! # Bounds
//!
//! Every structure here is built from tenant data, so every one is bounded:
//! runs per cohort ([`FateConfig::max_runs`]), nodes per build
//! ([`FateConfig::max_nodes`]), runs tracked by the fold, step records held
//! by it ([`MAX_STEP_RECORDS`]), steps read per run ([`RUN_STEP_WINDOW`]),
//! distinct signatures, cohorts, cached builds ([`CACHE_ENTRIES`]) — and a
//! build that meets a bound says so (`truncated`). Builds run under the
//! query deadline. Builds are tenant-scoped (the operator view, with no
//! tenant, keeps tenants apart in its cohorts), cached for
//! [`FateConfig::cache_ttl`], and every cached build is dropped by any
//! erasure: a map must not keep a run the store no longer holds.

pub mod http;
mod stats;

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Map;

use crate::attribution::{self, Outcome};
use crate::guard::{Action, NextStep, Verdict, VerdictDoc};
use crate::{semconv, Deadline, Error, Result, Span, SpanFilter, Store};

// ------------------------------------------------------------- constants

/// The window a map is learned from when the caller names none: the last
/// thirty days.
pub const DEFAULT_WINDOW_NS: u64 = 30 * 86_400 * 1_000_000_000;

/// Default for [`FateConfig::max_runs`] (`--fate-max-runs`).
pub const DEFAULT_MAX_RUNS: usize = 20_000;

/// Default for [`FateConfig::max_nodes`] (`--fate-max-nodes`).
pub const DEFAULT_MAX_NODES: usize = 50_000;

/// Default for [`FateConfig::cache_ttl`] (`--fate-cache-seconds`).
pub const DEFAULT_CACHE_SECONDS: u64 = 300;

/// Tokens in a path at most; deeper behaviour folds into [`FOLD_TOKEN`].
pub const MAX_DEPTH: usize = 64;

/// The terminal token deeper behaviour folds into — also where a run's path
/// ends when only part of it could be read.
pub const FOLD_TOKEN: &str = "…";

/// Depth a map is rendered to when the caller names none.
pub const DEFAULT_DEPTH: usize = 12;

/// Visits below which a node is pruned from a rendered map, by default.
pub const DEFAULT_MIN_SUPPORT: u64 = 3;

/// Runs sharing a path below which foresight gives no verdict, by default —
/// also the default `foresight_min_support` of a predictive fuse.
pub const DEFAULT_VERDICT_SUPPORT: u64 = 30;

/// Recorded steps before a predictive fuse may act, by default
/// (`foresight_after_steps`).
pub const DEFAULT_AFTER_STEPS: u64 = 3;

/// Nodes a rendered map shows when the caller names no `max_nodes`.
pub const DEFAULT_MAP_NODES: usize = 500;

/// Nodes a rendered map shows at most.
pub const MAX_MAP_NODES: usize = 5_000;

/// The earliest steps of each run a build reads its path from. Enough for a
/// full-depth path of short repeats; a run with more steps ends in
/// [`FOLD_TOKEN`] past what was read.
pub const RUN_STEP_WINDOW: usize = 192;

/// Step records (32 bytes each) one build holds at most, across every run it
/// tracks: 64 MiB. Past it no new run is tracked and no run grows.
pub const MAX_STEP_RECORDS: usize = 2 * 1024 * 1024;

/// Cohorts one build keeps, the largest first.
pub const MAX_COHORTS: usize = 256;

/// Distinct step signatures (and, separately, distinct path tokens) one
/// build interns. Tool and model names are producer text, so their variety
/// is bounded too; past it a path ends in [`FOLD_TOKEN`].
pub const MAX_SIGNATURES: usize = 20_000;

/// Bytes of a signature's name kept in a token.
pub const MAX_NAME_BYTES: usize = 160;

/// Builds the cache holds, least recently used evicted first.
pub const CACHE_ENTRIES: usize = 8;

/// Spans of one run read to compute its foresight.
pub const FORESIGHT_RUN_SPANS: usize = 20_000;

/// Longest run id followed, as the run guard: a longer one is skipped.
const MAX_RUN_ID_BYTES: usize = 1024;

/// Distinct agent (and service) names one build interns.
const MAX_AGENTS: usize = 4_096;

/// Task keys counted per cohort.
const MAX_TASK_KEYS: usize = 4_096;

/// Tasks listed per cohort, and the runs a task needs to be listed.
const LISTED_TASKS: usize = 32;
const MIN_TASK_RUNS: u64 = 3;

/// Fatal forks and hotspots a map summarizes.
const SUMMARY_ROWS: usize = 10;

/// A fatal fork: the gap to its siblings, and the significance it needs.
const FORK_GAP: f64 = 0.25;
const FORK_ALPHA: f64 = 0.05;

/// A hotspot: bits of next-step entropy.
const HOTSPOT_BITS: f64 = 1.0;

/// Depths a map scores its own foresight at, and the support a node needs
/// to be used as a prediction there (below it, the base rate predicts).
const CALIBRATION_DEPTHS: [usize; 6] = [1, 2, 3, 5, 8, 12];
const CALIBRATION_SUPPORT: u64 = 10;
const RELIABILITY_DEPTH: usize = 3;

/// Children a foresight lists as what runs did next.
const NEXT_OPTIONS: usize = 5;

/// Demanded builds the warmer remembers, and builds it makes per call.
const DEMAND_KEYS: usize = 64;
const WARM_PER_CALL: usize = 2;

/// Spans or runs between deadline checks in a build's own loops.
const DEADLINE_STRIDE: usize = 1_024;

const NONE: u32 = u32::MAX;

// ------------------------------------------------------------ configuration

/// The Fate Map's bounds and cache, from the server's `--fate-*` flags.
#[derive(Clone, Debug, PartialEq)]
pub struct FateConfig {
    /// Completed runs one cohort's map is built from at most — the newest
    /// first (`--fate-max-runs`). The fold tracks at most four times this
    /// many runs across a build's cohorts.
    pub max_runs: usize,
    /// Tree nodes one build holds across its cohorts (`--fate-max-nodes`).
    /// Nodes are created shallowest first and most-visited first, so the
    /// bound cuts the long tail of deep, rare paths.
    pub max_nodes: usize,
    /// How long a build is served before it is rebuilt
    /// (`--fate-cache-seconds`). Zero disables the cache — and with it the
    /// predictive fuses, which read only cached maps.
    pub cache_ttl: Duration,
}

impl Default for FateConfig {
    fn default() -> Self {
        Self {
            max_runs: DEFAULT_MAX_RUNS,
            max_nodes: DEFAULT_MAX_NODES,
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_SECONDS),
        }
    }
}

// ------------------------------------------------------------- wire types

/// The window a map was learned from, Unix nanoseconds, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct WindowDoc {
    /// Earliest span start considered.
    pub since_ns: u64,
    /// Latest span start considered.
    pub until_ns: u64,
}

/// One cohort: an agent's runs in a tenant, optionally one task's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CohortSummary {
    /// The tenant; empty is the default tenant.
    pub tenant: String,
    /// The agent name (first step's agent, else the earliest span's
    /// service). Producer text.
    pub agent: String,
    /// The task key, for a task sub-cohort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Completed runs: success + failure + abandoned.
    pub runs: u64,
    /// Runs that succeeded.
    pub success: u64,
    /// Runs that failed.
    pub failure: u64,
    /// Runs that stopped without resolving.
    pub abandoned: u64,
    /// Runs excluded because their outcome cannot be told — still active,
    /// or a declared outcome Traza does not recognise.
    pub unknown: u64,
    /// `success / runs`, or `null` with no completed run.
    pub base_rate: Option<f64>,
}

/// The cohort listing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cohorts {
    /// Agent cohorts largest first, each followed by its listed task
    /// sub-cohorts.
    pub cohorts: Vec<CohortSummary>,
    /// The window.
    pub window: WindowDoc,
    /// When the build behind this answer was made, server clock.
    pub built_at_ns: u64,
    /// Whether a bound cut the build short (see [`Bounds`]).
    pub truncated: bool,
    /// What the build met.
    pub bounds: Bounds,
}

/// What a build met on its way: every field is zero (or false) for a build
/// that read everything it was asked to.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Bounds {
    /// Spans read by the fold.
    pub spans_read: u64,
    /// Runs the fold tracked.
    pub runs_tracked: u64,
    /// Spans of runs the fold could not track: past four times
    /// `--fate-max-runs` runs, past the step-record budget, a run id over
    /// 1,024 bytes, or an agent past the name bound.
    pub spans_skipped: u64,
    /// Runs whose path was read from their earliest steps only (more than
    /// [`RUN_STEP_WINDOW`] steps, or the step-record budget), ending in `…`.
    pub runs_partial: u64,
    /// Completed runs left out of their cohort's map by `--fate-max-runs`
    /// (the oldest go first).
    pub runs_dropped: u64,
    /// Cohorts dropped past [`MAX_COHORTS`] (the smallest go first).
    pub cohorts_dropped: u64,
    /// Whether `--fate-max-nodes` stopped the tree growing somewhere.
    pub node_cap: bool,
    /// Whether the signature or token bound folded some path early.
    pub signature_cap: bool,
    /// Whether the window held more runs than the fold may track, so the
    /// build narrowed it to its most recent part (`window.since_ns` says
    /// where it starts).
    pub window_narrowed: bool,
}

impl Bounds {
    fn truncated(&self) -> bool {
        self.spans_skipped > 0
            || self.runs_partial > 0
            || self.runs_dropped > 0
            || self.cohorts_dropped > 0
            || self.node_cap
            || self.signature_cap
            || self.window_narrowed
    }
}

/// An example run at a node.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExampleRun {
    /// The run id. Producer text.
    pub run: String,
    /// How it ended.
    pub outcome: Outcome,
}

/// How a child compares with its siblings, when both have support.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForkStat {
    /// Runs through the sibling branches, pooled.
    pub siblings_visits: u64,
    /// Their pooled success rate.
    pub siblings_success_rate: f64,
    /// Siblings' rate minus this branch's.
    pub gap: f64,
    /// Two-sided Fisher exact p-value for the difference.
    pub p_value: f64,
}

/// One node of a rendered map.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MapNode {
    /// The behaviour token (`kind:name`, a repeat bucket, or `…`); the root
    /// is `^`. Producer text.
    pub token: String,
    /// Tokens from the root.
    pub depth: usize,
    /// Completed runs whose path passes through here.
    pub visits: u64,
    /// Of those, successes.
    pub success: u64,
    /// Failures.
    pub failure: u64,
    /// Abandoned.
    pub abandoned: u64,
    /// Runs whose path ended exactly here.
    pub ended: u64,
    /// `success / visits`.
    pub success_rate: Option<f64>,
    /// The 95% Wilson interval of the success rate.
    pub ci95: [f64; 2],
    /// `failure / visits`.
    pub p_fail_given_here: Option<f64>,
    /// Success rate over the cohort's base rate.
    pub lift: Option<f64>,
    /// Bits of entropy in what runs did next (ending included).
    pub entropy: f64,
    /// See the module docs.
    pub hotspot: bool,
    /// See the module docs.
    pub fatal_fork: bool,
    /// The comparison with siblings, when both have support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<ForkStat>,
    /// Summed run cost of the runs through here, USD.
    pub cost_usd_sum: f64,
    /// Summed run duration, first span start to last span end.
    pub duration_ns_sum: u64,
    /// The newest run of each outcome through here.
    pub example_runs: Vec<ExampleRun>,
    /// Shown children, most visited first.
    pub children: Vec<MapNode>,
    /// Children not shown: below `min_support`, past `depth`, or past
    /// `max_nodes`.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_children: u64,
    /// Visits of the children not shown.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_visits: u64,
    /// Runs that went on past here along a branch the node bound kept from
    /// being built.
    #[serde(skip_serializing_if = "is_zero")]
    pub cut: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A fatal fork, summarized with the path that leads to it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForkSummary {
    /// Tokens from the root to the fork's branch, inclusive.
    pub path: Vec<String>,
    /// Runs down the branch.
    pub visits: u64,
    /// The branch's success rate.
    pub success_rate: f64,
    /// The comparison.
    pub fork: ForkStat,
}

/// A hotspot, summarized with its path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HotspotSummary {
    /// Tokens from the root to the node, inclusive.
    pub path: Vec<String>,
    /// Runs through it.
    pub visits: u64,
    /// Bits of next-step entropy.
    pub entropy: f64,
    /// The most common next tokens, most visited first.
    pub next: Vec<String>,
}

/// How well a map's foresight did against what happened, at one depth.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CalibrationPoint {
    /// Path tokens into a run at which it was predicted.
    pub depth: usize,
    /// Completed runs at least this deep.
    pub runs: u64,
    /// Of those, runs predicted by their node (at least 10 runs of the
    /// other half there); the rest are predicted by the base rate.
    pub covered: u64,
    /// Mean squared error of the predictions, each run predicted from the
    /// half of the cohort it is not in (two-fold cross-validation, split by
    /// a hash of the run id).
    pub brier: f64,
    /// The same for the base rate alone.
    pub baseline_brier: f64,
    /// `1 − brier / baseline_brier`: positive when the map beats the base
    /// rate. `null` when the base rate is already perfect.
    pub skill: Option<f64>,
}

/// One reliability bin: predicted against observed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReliabilityBin {
    /// Lower edge of the predicted-probability bin.
    pub low: f64,
    /// Upper edge (exclusive, except the last).
    pub high: f64,
    /// Runs predicted in the bin.
    pub runs: u64,
    /// Their mean prediction.
    pub predicted: f64,
    /// Their observed success rate.
    pub observed: f64,
}

/// A map's account of its own accuracy.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Calibration {
    /// Brier scores by depth.
    pub depths: Vec<CalibrationPoint>,
    /// Predicted against observed at depth 3, in tenths (empty bins
    /// omitted).
    pub reliability: Vec<ReliabilityBin>,
}

/// A rendered Fate Map.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FateMap {
    /// The tenant.
    pub tenant: String,
    /// The agent. Producer text.
    pub agent: String,
    /// The task key, when narrowed to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Completed runs in the map.
    pub runs: u64,
    /// Of those, successes.
    pub success: u64,
    /// Failures.
    pub failure: u64,
    /// Abandoned.
    pub abandoned: u64,
    /// The cohort's runs excluded as unknown.
    pub unknown: u64,
    /// `success / runs`.
    pub base_rate: Option<f64>,
    /// Whether a bound cut the build short (see `bounds`).
    pub truncated: bool,
    /// What the build met.
    pub bounds: Bounds,
    /// The window.
    pub window: WindowDoc,
    /// When the build was made.
    pub built_at_ns: u64,
    /// The rendering's `depth`.
    pub depth: usize,
    /// The rendering's `min_support`.
    pub min_support: u64,
    /// Nodes in the cohort's tree.
    pub nodes_total: u64,
    /// Nodes rendered under `root`.
    pub nodes_shown: u64,
    /// The strongest fatal forks within `depth`, most significant first.
    pub fatal_forks: Vec<ForkSummary>,
    /// The highest-entropy hotspots within `depth`.
    pub hotspots: Vec<HotspotSummary>,
    /// How good this map's foresight has been.
    pub calibration: Calibration,
    /// The tree.
    pub root: MapNode,
}

/// What foresight concludes about a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForesightVerdict {
    /// Runs on this path do not do measurably worse than the cohort.
    OnTrack,
    /// Runs on this path do worse than the cohort, with 95% confidence.
    AtRisk,
    /// Runs on this path fail more often than they succeed, with 95%
    /// confidence.
    LikelyFailure,
    /// The run took a step no earlier run of its agent took at that point.
    Novel,
    /// Too little history to say.
    InsufficientHistory,
}

impl ForesightVerdict {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnTrack => "on_track",
            Self::AtRisk => "at_risk",
            Self::LikelyFailure => "likely_failure",
            Self::Novel => "novel",
            Self::InsufficientHistory => "insufficient_history",
        }
    }
}

/// A node's statistics as seen by one run (itself left out).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeStats {
    /// The node's token.
    pub token: String,
    /// Its depth.
    pub depth: usize,
    /// Runs through it.
    pub visits: u64,
    /// Successes.
    pub success: u64,
    /// Failures.
    pub failure: u64,
    /// Abandoned.
    pub abandoned: u64,
    /// `success / visits`.
    pub success_rate: Option<f64>,
    /// 95% Wilson interval.
    pub ci95: [f64; 2],
    /// `failure / visits`.
    pub p_fail_given_here: Option<f64>,
    /// Over the base rate.
    pub lift: Option<f64>,
    /// Mean run cost through here, USD.
    pub cost_usd_avg: Option<f64>,
}

/// Where a run left the known paths.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Divergence {
    /// Tokens matched before the run left the map.
    pub depth: usize,
    /// What earlier runs most often did at that point.
    pub common_next: Vec<String>,
    /// What this run did instead.
    pub took: String,
}

/// One thing earlier runs did next, and how it went for them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NextOption {
    /// The token.
    pub token: String,
    /// Runs that did it.
    pub visits: u64,
    /// Their success rate.
    pub success_rate: f64,
    /// Its 95% Wilson interval.
    pub ci95: [f64; 2],
}

/// Foresight for one run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Foresight {
    /// The run.
    pub run: String,
    /// Its tenant.
    pub tenant: String,
    /// Its agent (cohort). Producer text.
    pub agent: String,
    /// Its behaviour path so far.
    pub path: Vec<String>,
    /// Steps recorded.
    pub steps: u64,
    /// Path tokens found in the map.
    pub matched_depth: usize,
    /// The deepest matched node, the run itself left out.
    pub node: Option<NodeStats>,
    /// Success rate of runs that took this path.
    pub p_success: Option<f64>,
    /// Its 95% Wilson interval.
    pub ci95: Option<[f64; 2]>,
    /// Runs behind the estimate.
    pub support: u64,
    /// The cohort's success rate, the run left out.
    pub base_rate: Option<f64>,
    /// The verdict.
    pub verdict: ForesightVerdict,
    /// The verdict in one sentence.
    pub explanation: String,
    /// Where the run left every known path, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence: Option<Divergence>,
    /// What earlier runs did next from here, most common first.
    pub next: Vec<NextOption>,
    /// The next token whose runs did best, when it is better than staying
    /// the course would suggest: the highest Wilson lower bound among
    /// children with at least `min_support` runs, above this node's rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_next: Option<String>,
    /// How the run itself has ended so far (`unknown` while it is live).
    pub outcome: Outcome,
    /// Completed runs in the cohort, the run left out.
    pub cohort_runs: u64,
    /// Whether the run was in the map and taken out of it.
    pub excluded_self: bool,
    /// Whether the run was read from its earliest spans only.
    pub partial: bool,
    /// The support a verdict needed.
    pub min_support: u64,
    /// When the map was built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map_built_at_ns: Option<u64>,
}

/// Foresight as a fuse verdict carries it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GuardForesight {
    /// The verdict.
    pub verdict: ForesightVerdict,
    /// Success rate of runs that took this path.
    pub p_success: Option<f64>,
    /// Its 95% Wilson interval.
    pub ci95: Option<[f64; 2]>,
    /// Runs behind it.
    pub support: u64,
    /// Of those, successes.
    pub successes: u64,
    /// The cohort's base rate.
    pub base_rate: Option<f64>,
    /// Path tokens found in the map.
    pub matched_depth: usize,
    /// The check's next tool, when the path was judged with it appended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_tool: Option<String>,
    /// Whether this foresight raised the policy's action.
    pub raised: bool,
    /// One sentence.
    pub explanation: String,
    /// When the map was built; absent when no map was cached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map_built_at_ns: Option<u64>,
}

/// What to render of a map.
#[derive(Clone, Debug, PartialEq)]
pub struct MapRequest {
    /// The agent (cohort).
    pub agent: String,
    /// A task key, to narrow to one task.
    pub task: Option<String>,
    /// An explicit window, or the default last thirty days.
    pub window: Option<(u64, u64)>,
    /// Depth to render, `1..=64`.
    pub depth: usize,
    /// Visits a node needs to be rendered.
    pub min_support: u64,
    /// Nodes rendered at most.
    pub max_nodes: usize,
}

impl MapRequest {
    /// A request for `agent` with every default.
    pub fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            task: None,
            window: None,
            depth: DEFAULT_DEPTH,
            min_support: DEFAULT_MIN_SUPPORT,
            max_nodes: DEFAULT_MAP_NODES,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.agent.is_empty() {
            return Err(Error::Invalid("agent is required".to_owned()));
        }
        if self.depth == 0 || self.depth > MAX_DEPTH + 1 {
            return Err(Error::Invalid(format!(
                "depth must be 1 to {}",
                MAX_DEPTH + 1
            )));
        }
        if self.min_support == 0 {
            return Err(Error::Invalid("min_support must be at least 1".to_owned()));
        }
        if self.max_nodes == 0 || self.max_nodes > MAX_MAP_NODES {
            return Err(Error::Invalid(format!(
                "max_nodes must be 1 to {MAX_MAP_NODES}"
            )));
        }
        validate_window(self.window)
    }
}

fn validate_window(window: Option<(u64, u64)>) -> Result<()> {
    match window {
        Some((since, until)) if since > until => {
            Err(Error::Invalid("since must not be after until".to_owned()))
        }
        _ => Ok(()),
    }
}

// --------------------------------------------------------------- tokens

/// The repeat bucket suffix for `count` consecutive occurrences.
pub fn bucket(count: usize) -> &'static str {
    match count {
        0 | 1 => "",
        2 => "×2",
        3..=5 => "×3-5",
        _ => "×6+",
    }
}

/// A signature's text as a token stem: its name cut to [`MAX_NAME_BYTES`]
/// at a character boundary, marked when cut.
fn stem(signature: &str) -> String {
    if signature.len() <= MAX_NAME_BYTES {
        return signature.to_owned();
    }
    let mut end = MAX_NAME_BYTES;
    while end > 0 && !signature.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &signature[..end])
}

/// Run-length groups of consecutive equal items.
fn groups<T: PartialEq + Clone>(items: impl IntoIterator<Item = T>) -> Vec<(T, usize)> {
    let mut out: Vec<(T, usize)> = Vec::new();
    for item in items {
        match out.last_mut() {
            Some((last, count)) if *last == item => *count += 1,
            _ => out.push((item, 1)),
        }
    }
    out
}

/// The behaviour path of run-length `groups` of signature stems, at most
/// [`MAX_DEPTH`] tokens then [`FOLD_TOKEN`]. `complete` is whether the
/// groups cover the whole run: when they do not, the last group may be
/// longer than it looks, so it is dropped (unless it is already `×6+`,
/// which cannot change) and the path ends in [`FOLD_TOKEN`].
pub(crate) fn path_of(groups: &[(String, usize)], complete: bool) -> Vec<String> {
    let mut usable = groups;
    if !complete {
        if let Some((_, count)) = groups.last() {
            if *count < 6 {
                usable = &groups[..groups.len() - 1];
            }
        }
    }
    let mut path: Vec<String> = usable
        .iter()
        .take(MAX_DEPTH)
        .map(|(stem, count)| format!("{stem}{}", bucket(*count)))
        .collect();
    if usable.len() > MAX_DEPTH || !complete {
        path.push(FOLD_TOKEN.to_owned());
    }
    path
}

/// The first eight bytes of `text`, big-endian: compares like the text does
/// on those bytes.
fn prefix8(text: &str) -> u64 {
    let mut bytes = [0_u8; 8];
    for (slot, byte) in bytes.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
    u64::from_be_bytes(bytes)
}

/// Step order as the fold keeps it: start, then trace and span id by their
/// first eight bytes. The canonical order on everything but a tie of start
/// time AND both eight-byte prefixes.
type OrderKey = (u64, u64, u64);

fn order_key(span: &Span) -> OrderKey {
    (
        span.start_time_ns,
        prefix8(&span.trace_id),
        prefix8(&span.span_id),
    )
}

/// The task a span declares: `traza.task`, else `session.goal` (and its
/// Traceloop alias), hashed to 16 hex characters.
fn task_key(attributes: &Map<String, serde_json::Value>) -> Option<String> {
    let value =
        semconv::attr_str(attributes, "traza.task").or_else(|| semconv::goal(attributes))?;
    Some(crate::payload::sha256_hex(value.as_bytes())[..16].to_owned())
}

// ------------------------------------------------------------- interning

/// A bounded string interner.
struct Interner {
    names: Vec<Arc<str>>,
    ids: HashMap<Arc<str>, u32>,
    cap: usize,
}

impl Interner {
    fn new(cap: usize) -> Self {
        Self {
            names: Vec::new(),
            ids: HashMap::new(),
            cap,
        }
    }

    /// The id of `name`, interning it when there is room; `None` past the
    /// bound.
    fn intern(&mut self, name: &str) -> Option<u32> {
        if let Some(id) = self.ids.get(name) {
            return Some(*id);
        }
        if self.names.len() >= self.cap {
            return None;
        }
        let id = self.names.len() as u32;
        let shared: Arc<str> = Arc::from(name);
        self.names.push(Arc::clone(&shared));
        self.ids.insert(shared, id);
        Some(id)
    }

    fn get(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    fn name(&self, id: u32) -> &str {
        self.names.get(id as usize).map_or("", |name| name)
    }
}

// ------------------------------------------------------------------ fold

/// A step as the fold holds it: 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StepRecord {
    key: OrderKey,
    signature: u32,
}

/// The span that ended last, as [`attribution::session_outcome`] reads it.
struct Tail {
    end: u64,
    trace: Box<str>,
    span: Box<str>,
    error: bool,
}

/// The last declared outcome.
struct Declared {
    end: u64,
    trace: Box<str>,
    span: Box<str>,
    text: Box<str>,
}

/// One run while the fold reads it.
struct RunAcc {
    /// The earliest steps, as a max-heap so the latest is the one dropped.
    steps: BinaryHeap<StepRecord>,
    step_count: u64,
    /// Whether some step was not kept for want of room.
    steps_dropped: bool,
    earliest: (OrderKey, u32),
    first_step: Option<(OrderKey, Option<u32>)>,
    task: Option<(OrderKey, String)>,
    tail: Tail,
    declared: Option<Declared>,
    cost_usd: f64,
    min_start: u64,
    max_end: u64,
}

/// The fold's state across a build.
struct Fold<'s> {
    store: &'s Store,
    runs: HashMap<String, HashMap<String, RunAcc>>,
    signatures: Interner,
    names: Interner,
    tracked: usize,
    max_tracked: usize,
    held: usize,
    bounds: Bounds,
    /// The window, for the histogram.
    since: u64,
    width: u128,
    /// Spans read per slice of the window, newest last: what a narrower
    /// window would hold, should this one overflow.
    histogram: [u64; HISTOGRAM_BUCKETS],
    /// Spans of tracked runs, for the spans-per-run estimate.
    tracked_spans: u64,
    /// Whether a run went untracked for want of room.
    overflowed: bool,
}

/// Slices of the window the fold counts spans in.
const HISTOGRAM_BUCKETS: usize = 256;

/// Folds a build makes at most, narrowing its window after each overflow.
const NARROWING_ATTEMPTS: usize = 4;

impl<'s> Fold<'s> {
    fn new(store: &'s Store, max_runs: usize, since: u64, until: u64) -> Self {
        Self {
            store,
            runs: HashMap::new(),
            signatures: Interner::new(MAX_SIGNATURES),
            names: Interner::new(MAX_AGENTS),
            tracked: 0,
            max_tracked: max_runs.saturating_mul(4),
            held: 0,
            bounds: Bounds::default(),
            since,
            width: u128::from(until.saturating_sub(since)) + 1,
            histogram: [0; HISTOGRAM_BUCKETS],
            tracked_spans: 0,
            overflowed: false,
        }
    }

    /// After an overflow: the latest slice boundary from which the window
    /// would hold about four fifths of the runs the fold may track, judged
    /// from the spans per slice and the spans per tracked run. When even
    /// the newest slice is too full, that slice — the next fold's slices
    /// are finer. `None` when no narrower window is possible.
    fn narrower_since(&self) -> Option<u64> {
        let per_run = (self.tracked_spans as f64 / self.tracked.max(1) as f64).max(1.0);
        let room = self.max_tracked as f64 * 0.8;
        let mut runs = 0.0;
        let mut cut = HISTOGRAM_BUCKETS - 1;
        for bucket in (0..HISTOGRAM_BUCKETS).rev() {
            runs += self.histogram[bucket] as f64 / per_run;
            if runs > room {
                break;
            }
            cut = bucket;
        }
        let offset = (cut as u128 * self.width / HISTOGRAM_BUCKETS as u128) as u64;
        (offset > 0).then(|| self.since + offset)
    }
}

/// Bytes of a declared outcome kept: enough for every word
/// [`attribution::session_outcome`] recognises.
const MAX_DECLARED_BYTES: usize = 64;

fn cut_to(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Fold<'_> {
    fn observe(&mut self, span: &Span) {
        self.bounds.spans_read += 1;
        let offset = u128::from(span.start_time_ns.saturating_sub(self.since));
        let bucket = (offset * HISTOGRAM_BUCKETS as u128 / self.width) as usize;
        self.histogram[bucket.min(HISTOGRAM_BUCKETS - 1)] += 1;
        let run = semconv::run_id(&span.attributes, &span.trace_id);
        if run.len() > MAX_RUN_ID_BYTES {
            self.bounds.spans_skipped += 1;
            return;
        }
        let key = order_key(span);
        let service = match self.names.intern(&span.service) {
            Some(id) => id,
            None => {
                self.bounds.spans_skipped += 1;
                return;
            }
        };
        let tenant_runs = match self.runs.get_mut(span.tenant.as_str()) {
            Some(runs) => runs,
            None => self.runs.entry(span.tenant.clone()).or_default(),
        };
        if !tenant_runs.contains_key(run.as_str()) {
            if self.tracked >= self.max_tracked || self.held >= MAX_STEP_RECORDS {
                self.bounds.spans_skipped += 1;
                self.overflowed = true;
                return;
            }
            self.tracked += 1;
            tenant_runs.insert(
                run.clone(),
                RunAcc {
                    steps: BinaryHeap::new(),
                    step_count: 0,
                    steps_dropped: false,
                    earliest: (key, service),
                    first_step: None,
                    task: None,
                    tail: Tail {
                        end: span.end_time_ns,
                        trace: span.trace_id.as_str().into(),
                        span: span.span_id.as_str().into(),
                        error: span.status.eq_ignore_ascii_case("error"),
                    },
                    declared: None,
                    cost_usd: 0.0,
                    min_start: span.start_time_ns,
                    max_end: span.end_time_ns,
                },
            );
        }
        let Some(acc) = tenant_runs.get_mut(run.as_str()) else {
            return;
        };
        // Facts only for spans of runs the fold tracks: an untracked run's
        // spans cost a run-id lookup and nothing more.
        let llm = self.store.facts(span);
        let (kind, signature, agent) = semconv::step_signature_with(span, &llm);
        self.tracked_spans += 1;
        acc.cost_usd += llm.cost_usd.unwrap_or(0.0);
        acc.min_start = acc.min_start.min(span.start_time_ns);
        acc.max_end = acc.max_end.max(span.end_time_ns);
        if key < acc.earliest.0 {
            acc.earliest = (key, service);
        }
        let ends_later = (
            span.end_time_ns,
            span.trace_id.as_str(),
            span.span_id.as_str(),
        ) > (acc.tail.end, &*acc.tail.trace, &*acc.tail.span);
        if ends_later {
            acc.tail = Tail {
                end: span.end_time_ns,
                trace: span.trace_id.as_str().into(),
                span: span.span_id.as_str().into(),
                error: span.status.eq_ignore_ascii_case("error"),
            };
        }
        if let Some(text) = semconv::outcome(&span.attributes) {
            let later = acc.declared.as_ref().map_or(true, |declared| {
                (
                    span.end_time_ns,
                    span.trace_id.as_str(),
                    span.span_id.as_str(),
                ) > (declared.end, &*declared.trace, &*declared.span)
            });
            if later {
                acc.declared = Some(Declared {
                    end: span.end_time_ns,
                    trace: span.trace_id.as_str().into(),
                    span: span.span_id.as_str().into(),
                    text: cut_to(&text, MAX_DECLARED_BYTES).into(),
                });
            }
        }
        if let Some(task) = task_key(&span.attributes) {
            if acc
                .task
                .as_ref()
                .map_or(true, |(earliest, _)| key < *earliest)
            {
                acc.task = Some((key, task));
            }
        }
        if !kind.is_step() {
            return;
        }
        acc.step_count += 1;
        let agent = agent.and_then(|agent| self.names.intern(&agent));
        if acc
            .first_step
            .as_ref()
            .map_or(true, |(first, _)| key < *first)
        {
            acc.first_step = Some((key, agent));
        }
        // Past the signature bound a step cannot be named. It keeps its
        // place, as an unnamed record, and the run's path ends before it.
        let signature = self
            .signatures
            .intern(&stem(&signature))
            .unwrap_or_else(|| {
                self.bounds.signature_cap = true;
                NONE
            });
        let record = StepRecord { key, signature };
        let room = acc.steps.len() < RUN_STEP_WINDOW && self.held < MAX_STEP_RECORDS;
        if room {
            acc.steps.push(record);
            self.held += 1;
        } else {
            acc.steps_dropped = true;
            // Full: keep the earliest, dropping whichever is latest.
            if let Some(latest) = acc.steps.peek() {
                if record < *latest {
                    acc.steps.pop();
                    acc.steps.push(record);
                }
            }
        }
    }
}

/// A run after the fold, ready for the tree.
struct RunFinal {
    tenant: Arc<str>,
    run: Arc<str>,
    agent: u32,
    task: Option<String>,
    outcome: Outcome,
    path: Vec<u32>,
    cost_usd: f64,
    duration_ns: u64,
    last_end: u64,
}

/// The outcome [`attribution::session_outcome`] gives a run whose last
/// span to end is `tail` and whose last declaration is `declared` — the two
/// spans it reads; every other span changes nothing it decides.
fn outcome_of(tail: &Tail, declared: Option<&Declared>, now_ns: u64) -> Outcome {
    let minimal = |trace: &str, span: &str, end: u64, status: &str| Span {
        trace_id: trace.to_owned(),
        span_id: span.to_owned(),
        tenant: String::new(),
        parent_span_id: None,
        name: String::new(),
        start_time_ns: end,
        end_time_ns: end,
        status: status.to_owned(),
        service: String::new(),
        attributes: Map::new(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    };
    let mut spans = vec![minimal(
        &tail.trace,
        &tail.span,
        tail.end,
        if tail.error { "error" } else { "ok" },
    )];
    if let Some(declared) = declared {
        let mut span = minimal(&declared.trace, &declared.span, declared.end, "ok");
        span.attributes.insert(
            semconv::OUTCOME_KEYS[0].to_owned(),
            serde_json::Value::String(declared.text.to_string()),
        );
        spans.push(span);
    }
    attribution::session_outcome(&spans, now_ns, crate::mcp::SESSION_IDLE_NS).outcome
}

// ------------------------------------------------------------------ tree

/// One node of a cohort's tree.
#[derive(Clone, Debug)]
struct Node {
    token: u32,
    parent: u32,
    depth: u16,
    visits: u32,
    success: u32,
    failure: u32,
    abandoned: u32,
    ended: u32,
    cut: u32,
    cost_usd_sum: f64,
    duration_ns_sum: u64,
    /// Newest run index per outcome: success, failure, abandoned.
    examples: [u32; 3],
    children: Vec<u32>,
}

impl Node {
    fn new(token: u32, parent: u32, depth: u16) -> Self {
        Self {
            token,
            parent,
            depth,
            visits: 0,
            success: 0,
            failure: 0,
            abandoned: 0,
            ended: 0,
            cut: 0,
            cost_usd_sum: 0.0,
            duration_ns_sum: 0,
            examples: [NONE; 3],
            children: Vec::new(),
        }
    }

    fn add(&mut self, run_index: u32, run: &RunFinal) {
        self.visits += 1;
        let slot = match run.outcome {
            Outcome::Success => {
                self.success += 1;
                0
            }
            Outcome::Failure => {
                self.failure += 1;
                1
            }
            _ => {
                self.abandoned += 1;
                2
            }
        };
        if self.examples[slot] == NONE {
            self.examples[slot] = run_index;
        }
        self.cost_usd_sum += run.cost_usd;
        self.duration_ns_sum = self.duration_ns_sum.saturating_add(run.duration_ns);
    }

    fn counts(&self) -> Counts {
        Counts {
            visits: u64::from(self.visits),
            success: u64::from(self.success),
            failure: u64::from(self.failure),
            abandoned: u64::from(self.abandoned),
        }
    }
}

/// Outcome counts, possibly with one run left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    visits: u64,
    success: u64,
    failure: u64,
    abandoned: u64,
}

impl Counts {
    fn without(mut self, outcome: Outcome) -> Self {
        if self.visits == 0 {
            return self;
        }
        self.visits -= 1;
        match outcome {
            Outcome::Success => self.success = self.success.saturating_sub(1),
            Outcome::Failure => self.failure = self.failure.saturating_sub(1),
            _ => self.abandoned = self.abandoned.saturating_sub(1),
        }
        self
    }

    fn rate(&self) -> Option<f64> {
        (self.visits > 0).then(|| self.success as f64 / self.visits as f64)
    }
}

/// A run's membership in its cohort's map: how it ended, and the deepest
/// node its path reached — its whole contribution, for leave-one-out.
#[derive(Clone, Copy, Debug)]
struct Member {
    outcome: Outcome,
    deepest: u32,
}

/// One cohort's map.
struct Cohort {
    tenant: String,
    agent: String,
    success: u64,
    failure: u64,
    abandoned: u64,
    unknown: u64,
    tasks: Vec<CohortSummary>,
    nodes: Vec<Node>,
    run_ids: Vec<Arc<str>>,
    members: HashMap<Arc<str>, Member>,
    calibration: Calibration,
}

impl Cohort {
    fn summary(&self, task: Option<&str>) -> CohortSummary {
        let runs = self.success + self.failure + self.abandoned;
        CohortSummary {
            tenant: self.tenant.clone(),
            agent: self.agent.clone(),
            task: task.map(str::to_owned),
            runs,
            success: self.success,
            failure: self.failure,
            abandoned: self.abandoned,
            unknown: self.unknown,
            base_rate: (runs > 0).then(|| stats::round4(self.success as f64 / runs as f64)),
        }
    }

    /// The node ids from the root down to `deepest`.
    fn chain(&self, deepest: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut at = deepest;
        while at != NONE {
            chain.push(at);
            at = self.nodes.get(at as usize).map_or(NONE, |node| node.parent);
        }
        chain.reverse();
        chain
    }
}

/// Grows a cohort's tree from `runs` (newest first) level by level, the
/// most-visited children of each level first, while `budget` lasts.
fn grow(runs: &[RunFinal], budget: &mut usize, bounds: &mut Bounds) -> (Vec<Node>, Vec<u32>) {
    let mut nodes = vec![Node::new(NONE, NONE, 0)];
    let mut at: Vec<u32> = vec![0; runs.len()];
    let mut deepest: Vec<u32> = vec![0; runs.len()];
    for (index, run) in runs.iter().enumerate() {
        nodes[0].add(index as u32, run);
        if run.path.is_empty() {
            nodes[0].ended += 1;
            at[index] = NONE;
        }
    }
    for depth in 0..=MAX_DEPTH {
        let mut wanted: HashMap<(u32, u32), u32> = HashMap::new();
        for (index, run) in runs.iter().enumerate() {
            if at[index] != NONE {
                *wanted.entry((at[index], run.path[depth])).or_default() += 1;
            }
        }
        if wanted.is_empty() {
            break;
        }
        let mut order: Vec<((u32, u32), u32)> = wanted.into_iter().collect();
        order.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        let mut created: HashMap<(u32, u32), u32> = HashMap::with_capacity(order.len());
        for ((parent, token), _) in order {
            if *budget == 0 {
                bounds.node_cap = true;
                break;
            }
            *budget -= 1;
            let id = nodes.len() as u32;
            nodes.push(Node::new(token, parent, (depth + 1) as u16));
            nodes[parent as usize].children.push(id);
            created.insert((parent, token), id);
        }
        for (index, run) in runs.iter().enumerate() {
            let parent = at[index];
            if parent == NONE {
                continue;
            }
            match created.get(&(parent, run.path[depth])) {
                Some(&child) => {
                    nodes[child as usize].add(index as u32, run);
                    deepest[index] = child;
                    if run.path.len() == depth + 1 {
                        nodes[child as usize].ended += 1;
                        at[index] = NONE;
                    } else {
                        at[index] = child;
                    }
                }
                None => {
                    nodes[parent as usize].cut += 1;
                    at[index] = NONE;
                }
            }
        }
    }
    (nodes, deepest)
}

/// Scores cross-validated predictions against outcomes; see
/// [`CalibrationPoint`].
fn calibrate(nodes: &[Node], runs: &[RunFinal], deepest: &[u32]) -> Calibration {
    if runs.len() < 2 {
        return Calibration::default();
    }
    let chains: Vec<Vec<u32>> = deepest
        .iter()
        .map(|&deep| {
            let mut chain = Vec::new();
            let mut at = deep;
            while at != NONE {
                chain.push(at);
                at = nodes[at as usize].parent;
            }
            chain.reverse();
            chain
        })
        .collect();
    // Two-fold cross-validation: each run is predicted from the half of the
    // cohort it is not in. Leave-one-out would be tighter, but on a branch
    // of identical runs it predicts a success lower and a failure higher —
    // its own outcome shifts the rate it is scored against — which splits a
    // reliability table along exactly the line it is trying to draw.
    let folds: Vec<usize> = runs.iter().map(|run| fold_of(&run.run)).collect();
    let mut counts: Vec<[(u64, u64); 2]> = vec![[(0, 0); 2]; nodes.len()];
    for (index, run) in runs.iter().enumerate() {
        let success = u64::from(run.outcome == Outcome::Success);
        for &id in &chains[index] {
            let slot = &mut counts[id as usize][folds[index]];
            slot.0 += 1;
            slot.1 += success;
        }
    }
    let rate = |(visits, success): (u64, u64)| success as f64 / visits as f64;
    let mut depths = Vec::new();
    let mut bins: [(u64, f64, u64); 10] = [(0, 0.0, 0); 10];
    for depth in CALIBRATION_DEPTHS {
        let (mut runs_at, mut covered, mut brier, mut baseline) = (0_u64, 0_u64, 0.0, 0.0);
        for (index, run) in runs.iter().enumerate() {
            let other = 1 - folds[index];
            if run.path.len() < depth || counts[0][other].0 == 0 {
                continue;
            }
            runs_at += 1;
            let y = if run.outcome == Outcome::Success {
                1.0
            } else {
                0.0
            };
            let base = rate(counts[0][other]);
            let node = chains[index]
                .get(depth)
                .map(|&id| counts[id as usize][other])
                .filter(|(visits, _)| *visits >= CALIBRATION_SUPPORT);
            let predicted = match node {
                Some(node) => {
                    covered += 1;
                    rate(node)
                }
                None => base,
            };
            brier += (predicted - y) * (predicted - y);
            baseline += (base - y) * (base - y);
            if depth == RELIABILITY_DEPTH {
                let bin = ((predicted * 10.0) as usize).min(9);
                bins[bin].0 += 1;
                bins[bin].1 += predicted;
                bins[bin].2 += u64::from(run.outcome == Outcome::Success);
            }
        }
        if runs_at == 0 {
            continue;
        }
        let n = runs_at as f64;
        let (brier, baseline) = (brier / n, baseline / n);
        depths.push(CalibrationPoint {
            depth,
            runs: runs_at,
            covered,
            brier: stats::round4(brier),
            baseline_brier: stats::round4(baseline),
            skill: (baseline > 0.0).then(|| stats::round4(1.0 - brier / baseline)),
        });
    }
    let reliability = bins
        .iter()
        .enumerate()
        .filter(|(_, bin)| bin.0 > 0)
        .map(|(index, bin)| ReliabilityBin {
            low: index as f64 / 10.0,
            high: (index + 1) as f64 / 10.0,
            runs: bin.0,
            predicted: stats::round4(bin.1 / bin.0 as f64),
            observed: stats::round4(bin.2 as f64 / bin.0 as f64),
        })
        .collect();
    Calibration {
        depths,
        reliability,
    }
}

/// Which half of a cohort a run falls in for cross-validation: FNV-1a of its
/// id, so the split is deterministic and blind to how the run ended.
fn fold_of(run: &str) -> usize {
    let hash = run.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    (hash & 1) as usize
}

// ----------------------------------------------------------------- build

/// Which window a build covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Window {
    /// The last [`DEFAULT_WINDOW_NS`], as of the build.
    Default,
    /// An explicit `[since, until]`.
    Range(u64, u64),
}

/// What a build is of.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BuildKey {
    /// The tenant, or `None` for the operator view of every tenant.
    pub(crate) scope: Option<String>,
    /// Only runs of this task key, when set.
    pub(crate) task: Option<String>,
    pub(crate) window: Window,
}

impl BuildKey {
    fn of(scope: Option<&str>, task: Option<&str>, window: Option<(u64, u64)>) -> Self {
        Self {
            scope: scope.map(str::to_owned),
            task: task.map(str::to_owned),
            window: window.map_or(Window::Default, |(since, until)| {
                Window::Range(since, until)
            }),
        }
    }
}

/// Every cohort of one scope and window.
pub(crate) struct Build {
    built_at_ns: u64,
    window: WindowDoc,
    tokens: Interner,
    cohorts: Vec<Cohort>,
    index: HashMap<(String, String), usize>,
    bounds: Bounds,
}

impl Build {
    fn cohort(&self, tenant: &str, agent: &str) -> Option<&Cohort> {
        self.index
            .get(&(tenant.to_owned(), agent.to_owned()))
            .and_then(|index| self.cohorts.get(*index))
    }

    fn token(&self, id: u32) -> &str {
        if id == NONE {
            "^"
        } else {
            self.tokens.name(id)
        }
    }
}

/// A cohort while a build groups runs: tenant and interned agent name.
type CohortKey = (Arc<str>, u32);

/// Builds the cohorts `key` describes, as of `now_ns`.
fn build(
    store: &Store,
    key: &BuildKey,
    config: &FateConfig,
    now_ns: u64,
    deadline: Option<Deadline>,
) -> Result<Build> {
    let (mut since_ns, until_ns) = match key.window {
        Window::Default => (now_ns.saturating_sub(DEFAULT_WINDOW_NS), now_ns),
        Window::Range(since, until) => (since, until),
    };
    let max_runs = config.max_runs.max(1);
    // A window holding more runs than the fold may track is narrowed to its
    // most recent part that fits, so the map keeps the newest runs rather
    // than whichever the fold happened to meet first — and says so.
    let mut narrowed = false;
    let mut attempts = 0;
    let fold = loop {
        let filter = SpanFilter {
            tenant: key.scope.clone(),
            since_ns: Some(since_ns),
            until_ns: Some(until_ns),
            ..SpanFilter::default()
        };
        let mut fold = Fold::new(store, max_runs, since_ns, until_ns);
        store.fold_spans_under(&filter, deadline, |span| fold.observe(span))?;
        attempts += 1;
        if !fold.overflowed || attempts >= NARROWING_ATTEMPTS {
            break fold;
        }
        match fold.narrower_since() {
            Some(since) if since > since_ns => {
                since_ns = since;
                narrowed = true;
            }
            _ => break fold,
        }
    };
    let Fold {
        runs: accs,
        signatures,
        names,
        mut bounds,
        tracked,
        ..
    } = fold;
    bounds.runs_tracked = tracked as u64;
    bounds.window_narrowed = narrowed;

    // Runs, finalized: outcome, cohort, path.
    let mut tokens = Interner::new(MAX_SIGNATURES);
    let mut finals: Vec<RunFinal> = Vec::with_capacity(tracked);
    let mut seen = 0_usize;
    let mut tenants: Vec<(String, HashMap<String, RunAcc>)> = accs.into_iter().collect();
    tenants.sort_by(|left, right| left.0.cmp(&right.0));
    for (tenant, runs) in tenants {
        let tenant: Arc<str> = Arc::from(tenant.as_str());
        let mut runs: Vec<(String, RunAcc)> = runs.into_iter().collect();
        runs.sort_by(|left, right| left.0.cmp(&right.0));
        for (run, acc) in runs {
            seen += 1;
            if seen % DEADLINE_STRIDE == 0 {
                Deadline::check(deadline, 0)?;
            }
            let task = acc.task.map(|(_, task)| task);
            if key.task.is_some() && task != key.task {
                continue;
            }
            let agent = acc
                .first_step
                .and_then(|(_, agent)| agent)
                .unwrap_or(acc.earliest.1);
            let outcome = outcome_of(&acc.tail, acc.declared.as_ref(), now_ns);
            let mut complete = !acc.steps_dropped && acc.step_count == acc.steps.len() as u64;
            let mut ordered = acc.steps.into_sorted_vec();
            if let Some(unnamed) = ordered.iter().position(|record| record.signature == NONE) {
                ordered.truncate(unnamed);
                complete = false;
            }
            if !complete {
                bounds.runs_partial += 1;
            }
            let grouped: Vec<(String, usize)> =
                groups(ordered.iter().map(|record| record.signature))
                    .into_iter()
                    .map(|(signature, count)| (signatures.name(signature).to_owned(), count))
                    .collect();
            let mut path = Vec::new();
            for token in path_of(&grouped, complete) {
                match tokens.intern(&token) {
                    Some(id) => path.push(id),
                    None => {
                        bounds.signature_cap = true;
                        if let Some(fold_id) = tokens.intern(FOLD_TOKEN) {
                            path.push(fold_id);
                        }
                        break;
                    }
                }
            }
            finals.push(RunFinal {
                tenant: Arc::clone(&tenant),
                run: Arc::from(run.as_str()),
                agent,
                task,
                outcome,
                path,
                cost_usd: acc.cost_usd,
                duration_ns: acc.max_end.saturating_sub(acc.min_start),
                last_end: acc.max_end,
            });
        }
    }

    // Cohorts: (tenant, agent), the largest first.
    let mut grouped: HashMap<CohortKey, Vec<RunFinal>> = HashMap::new();
    for run in finals {
        grouped
            .entry((Arc::clone(&run.tenant), run.agent))
            .or_default()
            .push(run);
    }
    let mut cohorts_in: Vec<(CohortKey, Vec<RunFinal>)> = grouped.into_iter().collect();
    cohorts_in.sort_by(|left, right| {
        right
            .1
            .len()
            .cmp(&left.1.len())
            .then_with(|| left.0 .0.cmp(&right.0 .0))
            .then_with(|| names.name(left.0 .1).cmp(names.name(right.0 .1)))
    });
    if cohorts_in.len() > MAX_COHORTS {
        bounds.cohorts_dropped = (cohorts_in.len() - MAX_COHORTS) as u64;
        cohorts_in.truncate(MAX_COHORTS);
    }
    let mut budget = config.max_nodes.max(1);
    let mut cohorts = Vec::with_capacity(cohorts_in.len());
    let mut index = HashMap::new();
    for ((tenant, agent), runs) in cohorts_in {
        Deadline::check(deadline, 0)?;
        let mut unknown = 0_u64;
        let mut task_counts: HashMap<String, [u64; 4]> = HashMap::new();
        let mut known: Vec<RunFinal> = Vec::with_capacity(runs.len());
        for run in runs {
            if key.task.is_none() {
                if let Some(task) = &run.task {
                    let slot = match run.outcome {
                        Outcome::Success => 0,
                        Outcome::Failure => 1,
                        Outcome::Abandoned => 2,
                        Outcome::Unknown => 3,
                    };
                    if let Some(counts) = task_counts.get_mut(task) {
                        counts[slot] += 1;
                    } else if task_counts.len() < MAX_TASK_KEYS {
                        let mut counts = [0; 4];
                        counts[slot] += 1;
                        task_counts.insert(task.clone(), counts);
                    }
                }
            }
            if run.outcome == Outcome::Unknown {
                unknown += 1;
            } else {
                known.push(run);
            }
        }
        // Newest first, so the cap drops the oldest and examples are the
        // newest of their outcome.
        known.sort_by(|left, right| {
            right
                .last_end
                .cmp(&left.last_end)
                .then_with(|| left.run.cmp(&right.run))
        });
        if known.len() > max_runs {
            bounds.runs_dropped += (known.len() - max_runs) as u64;
            known.truncate(max_runs);
        }
        let (nodes, deepest) = grow(&known, &mut budget, &mut bounds);
        let calibration = calibrate(&nodes, &known, &deepest);
        let mut tasks: Vec<CohortSummary> = task_counts
            .into_iter()
            .filter(|(_, counts)| counts[0] + counts[1] + counts[2] >= MIN_TASK_RUNS)
            .map(|(task, counts)| {
                let runs = counts[0] + counts[1] + counts[2];
                CohortSummary {
                    tenant: tenant.to_string(),
                    agent: names.name(agent).to_owned(),
                    task: Some(task),
                    runs,
                    success: counts[0],
                    failure: counts[1],
                    abandoned: counts[2],
                    unknown: counts[3],
                    base_rate: Some(stats::round4(counts[0] as f64 / runs as f64)),
                }
            })
            .collect();
        tasks.sort_by(|left, right| right.runs.cmp(&left.runs).then(left.task.cmp(&right.task)));
        tasks.truncate(LISTED_TASKS);
        let root = nodes[0].counts();
        let members = known
            .iter()
            .zip(&deepest)
            .map(|(run, &deep)| {
                (
                    Arc::clone(&run.run),
                    Member {
                        outcome: run.outcome,
                        deepest: deep,
                    },
                )
            })
            .collect();
        let run_ids = known.iter().map(|run| Arc::clone(&run.run)).collect();
        let cohort = Cohort {
            tenant: tenant.to_string(),
            agent: names.name(agent).to_owned(),
            success: root.success,
            failure: root.failure,
            abandoned: root.abandoned,
            unknown,
            tasks,
            nodes,
            run_ids,
            members,
            calibration,
        };
        index.insert((cohort.tenant.clone(), cohort.agent.clone()), cohorts.len());
        cohorts.push(cohort);
    }
    Deadline::check(deadline, 0)?;
    Ok(Build {
        built_at_ns: now_ns,
        window: WindowDoc { since_ns, until_ns },
        tokens,
        cohorts,
        index,
        bounds,
    })
}

// ----------------------------------------------------------------- cache

/// Cached builds, single-flight per key, dropped wholesale by any erasure.
pub(crate) struct FateCache {
    config: FateConfig,
    state: Mutex<CacheState>,
    settled: Condvar,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<BuildKey, Entry>,
    building: HashSet<BuildKey>,
    demand: Vec<BuildKey>,
    /// Keys whose last background build failed, and when: retried after a
    /// cache lifetime, not every tick.
    failed: HashMap<BuildKey, Instant>,
    /// Bumped by every erasure: a build that began before one is not
    /// installed.
    epoch: u64,
    tick: u64,
}

struct Entry {
    build: Arc<Build>,
    at: Instant,
    used: u64,
}

/// Clears a key's in-flight mark however the build ends, a panic included,
/// so waiters are never left waiting on a build nobody is running.
struct InFlight<'c> {
    cache: &'c FateCache,
    key: BuildKey,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cache.state.lock() {
            state.building.remove(&self.key);
        }
        self.cache.settled.notify_all();
    }
}

impl FateCache {
    pub(crate) fn new(config: &FateConfig) -> Self {
        Self {
            config: config.clone(),
            state: Mutex::new(CacheState::default()),
            settled: Condvar::new(),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, CacheState>> {
        self.state
            .lock()
            .map_err(|_| Error::LockPoisoned("fate cache"))
    }

    fn fresh(&self, entry: &Entry) -> bool {
        !self.config.cache_ttl.is_zero() && entry.at.elapsed() < self.config.cache_ttl
    }

    /// The build for `key`: cached while fresh, else built under `deadline`
    /// — once, however many requests ask at the same moment.
    fn get_or_build(
        &self,
        store: &Store,
        key: &BuildKey,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<Arc<Build>> {
        let mut state = self.lock()?;
        loop {
            state.tick += 1;
            let tick = state.tick;
            let ttl = self.config.cache_ttl;
            if let Some(entry) = state.entries.get_mut(key) {
                if !ttl.is_zero() && entry.at.elapsed() < ttl {
                    entry.used = tick;
                    return Ok(Arc::clone(&entry.build));
                }
            }
            if !state.building.contains(key) {
                break;
            }
            let wait = match deadline {
                Some(deadline) => deadline.at.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(60),
            };
            if wait.is_zero() {
                Deadline::check(deadline, 0)?;
            }
            state = self
                .settled
                .wait_timeout(state, wait)
                .map_err(|_| Error::LockPoisoned("fate cache"))?
                .0;
        }
        state.building.insert(key.clone());
        let epoch = state.epoch;
        drop(state);
        let in_flight = InFlight {
            cache: self,
            key: key.clone(),
        };
        let built = Arc::new(build(store, key, &self.config, now_ns, deadline)?);
        let mut state = self.lock()?;
        if state.epoch == epoch && !self.config.cache_ttl.is_zero() {
            state.tick += 1;
            let used = state.tick;
            state.failed.remove(key);
            state.entries.insert(
                key.clone(),
                Entry {
                    build: Arc::clone(&built),
                    at: Instant::now(),
                    used,
                },
            );
            while state.entries.len() > CACHE_ENTRIES {
                let oldest = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone());
                match oldest {
                    Some(oldest) => {
                        state.entries.remove(&oldest);
                    }
                    None => break,
                }
            }
        }
        drop(state);
        drop(in_flight);
        Ok(built)
    }

    /// The cached build for `key` without building: served for up to twice
    /// the cache lifetime, and asked to be rebuilt in the background once
    /// past one lifetime (or when absent).
    fn peek(&self, key: &BuildKey) -> Option<Arc<Build>> {
        let mut state = self.state.lock().ok()?;
        let ttl = self.config.cache_ttl;
        if ttl.is_zero() {
            return None;
        }
        let (found, stale) = match state.entries.get(key) {
            Some(entry) => {
                let age = entry.at.elapsed();
                (
                    (age < ttl.saturating_mul(2)).then(|| Arc::clone(&entry.build)),
                    age >= ttl,
                )
            }
            None => (None, true),
        };
        if stale && !state.demand.contains(key) && state.demand.len() < DEMAND_KEYS {
            state.demand.push(key.clone());
        }
        found
    }

    /// Drops every cached build, and keeps any build in flight from being
    /// installed.
    pub(crate) fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.epoch += 1;
            state.entries.clear();
        }
    }

    fn take_demand(&self, count: usize) -> Vec<BuildKey> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let ttl = self.config.cache_ttl;
        let mut taken = Vec::new();
        let mut kept = Vec::new();
        for key in std::mem::take(&mut state.demand) {
            let backing_off = state
                .failed
                .get(&key)
                .is_some_and(|at| at.elapsed() < ttl.max(Duration::from_secs(60)));
            let fresh = state
                .entries
                .get(&key)
                .is_some_and(|entry| self.fresh(entry));
            if fresh || backing_off {
                continue;
            }
            if taken.len() < count {
                taken.push(key);
            } else {
                kept.push(key);
            }
        }
        state.demand = kept;
        taken
    }

    fn failed(&self, key: &BuildKey) {
        if let Ok(mut state) = self.state.lock() {
            if state.failed.len() >= DEMAND_KEYS {
                state.failed.clear();
            }
            state.failed.insert(key.clone(), Instant::now());
        }
    }
}

// ------------------------------------------------------------- rendering

/// Per-request rendering context.
struct Render<'b> {
    build: &'b Build,
    cohort: &'b Cohort,
    base_rate: Option<f64>,
    min_support: u64,
    facts: stats::LnFactorials,
}

impl Render<'_> {
    fn path(&self, id: u32) -> Vec<String> {
        self.cohort
            .chain(id)
            .into_iter()
            .skip(1)
            .map(|node| {
                self.build
                    .token(self.cohort.nodes[node as usize].token)
                    .to_owned()
            })
            .collect()
    }

    fn entropy(&self, node: &Node) -> f64 {
        let mut counts: Vec<u64> = node
            .children
            .iter()
            .map(|&child| u64::from(self.cohort.nodes[child as usize].visits))
            .collect();
        counts.push(u64::from(node.ended));
        stats::entropy(&counts)
    }

    fn fork(&self, id: u32) -> Option<ForkStat> {
        let node = &self.cohort.nodes[id as usize];
        if node.parent == NONE {
            return None;
        }
        let parent = &self.cohort.nodes[node.parent as usize];
        let (mut visits, mut success) = (0_u64, 0_u64);
        for &sibling in &parent.children {
            if sibling != id {
                let sibling = &self.cohort.nodes[sibling as usize];
                visits += u64::from(sibling.visits);
                success += u64::from(sibling.success);
            }
        }
        let own = node.counts();
        if own.visits < self.min_support || visits < self.min_support {
            return None;
        }
        let own_rate = own.rate().unwrap_or(0.0);
        let siblings_rate = success as f64 / visits as f64;
        let p_value = stats::fisher_two_sided(
            own.success,
            own.visits - own.success,
            success,
            visits - success,
            &self.facts,
        );
        Some(ForkStat {
            siblings_visits: visits,
            siblings_success_rate: stats::round4(siblings_rate),
            gap: stats::round4(siblings_rate - own_rate),
            p_value,
        })
    }

    fn is_fatal(fork: &ForkStat) -> bool {
        fork.gap >= FORK_GAP && fork.p_value < FORK_ALPHA
    }

    fn node(&self, id: u32, shown: &HashSet<u32>) -> MapNode {
        let node = &self.cohort.nodes[id as usize];
        let counts = node.counts();
        let (low, high) = stats::wilson(counts.success, counts.visits);
        let rate = counts.rate();
        let entropy = self.entropy(node);
        let fork = self.fork(id);
        let mut children: Vec<u32> = node.children.clone();
        children.sort_by(|left, right| {
            let (l, r) = (
                &self.cohort.nodes[*left as usize],
                &self.cohort.nodes[*right as usize],
            );
            r.visits
                .cmp(&l.visits)
                .then_with(|| self.build.token(l.token).cmp(self.build.token(r.token)))
        });
        let (mut omitted_children, mut omitted_visits) = (0, 0);
        let mut rendered = Vec::new();
        for child in children {
            if shown.contains(&child) {
                rendered.push(self.node(child, shown));
            } else {
                omitted_children += 1;
                omitted_visits += u64::from(self.cohort.nodes[child as usize].visits);
            }
        }
        let outcomes = [Outcome::Success, Outcome::Failure, Outcome::Abandoned];
        let example_runs = node
            .examples
            .iter()
            .zip(outcomes)
            .filter(|(index, _)| **index != NONE)
            .map(|(index, outcome)| ExampleRun {
                run: self.cohort.run_ids[*index as usize].to_string(),
                outcome,
            })
            .collect();
        MapNode {
            token: self.build.token(node.token).to_owned(),
            depth: usize::from(node.depth),
            visits: counts.visits,
            success: counts.success,
            failure: counts.failure,
            abandoned: counts.abandoned,
            ended: u64::from(node.ended),
            success_rate: rate.map(stats::round4),
            ci95: [stats::round4(low), stats::round4(high)],
            p_fail_given_here: (counts.visits > 0)
                .then(|| stats::round4(counts.failure as f64 / counts.visits as f64)),
            lift: match (rate, self.base_rate) {
                (Some(rate), Some(base)) if base > 0.0 => Some(stats::round4(rate / base)),
                _ => None,
            },
            entropy: stats::round4(entropy),
            hotspot: entropy >= HOTSPOT_BITS && counts.visits >= self.min_support,
            fatal_fork: fork.as_ref().is_some_and(Self::is_fatal),
            fork,
            cost_usd_sum: round_usd(node.cost_usd_sum),
            duration_ns_sum: node.duration_ns_sum,
            example_runs,
            children: rendered,
            omitted_children,
            omitted_visits,
            cut: u64::from(node.cut),
        }
    }
}

fn render_map(build: &Build, cohort: &Cohort, request: &MapRequest) -> FateMap {
    let summary = cohort.summary(request.task.as_deref());
    let render = Render {
        build,
        cohort,
        base_rate: summary.base_rate,
        min_support: request.min_support,
        facts: stats::LnFactorials::up_to(summary.runs),
    };
    // The nodes shown: the root, then the most visited of those whose
    // parent is shown, within depth and support, up to max_nodes.
    let mut shown: HashSet<u32> = HashSet::from([0]);
    let mut frontier: BinaryHeap<(u32, std::cmp::Reverse<u32>)> = BinaryHeap::new();
    let push_children = |frontier: &mut BinaryHeap<(u32, std::cmp::Reverse<u32>)>, id: u32| {
        for &child in &cohort.nodes[id as usize].children {
            let node = &cohort.nodes[child as usize];
            if usize::from(node.depth) <= request.depth
                && u64::from(node.visits) >= request.min_support
            {
                frontier.push((node.visits, std::cmp::Reverse(child)));
            }
        }
    };
    push_children(&mut frontier, 0);
    while shown.len() < request.max_nodes + 1 {
        let Some((_, std::cmp::Reverse(id))) = frontier.pop() else {
            break;
        };
        shown.insert(id);
        push_children(&mut frontier, id);
    }
    // Summaries over every node within depth and support, shown or not.
    let mut forks = Vec::new();
    let mut hotspots = Vec::new();
    for (id, node) in cohort.nodes.iter().enumerate().skip(1) {
        if usize::from(node.depth) > request.depth || u64::from(node.visits) < request.min_support {
            continue;
        }
        if let Some(fork) = render.fork(id as u32) {
            if Render::is_fatal(&fork) {
                forks.push(ForkSummary {
                    path: render.path(id as u32),
                    visits: u64::from(node.visits),
                    success_rate: stats::round4(node.counts().rate().unwrap_or(0.0)),
                    fork,
                });
            }
        }
    }
    for (id, node) in cohort.nodes.iter().enumerate() {
        if usize::from(node.depth) >= request.depth || u64::from(node.visits) < request.min_support
        {
            continue;
        }
        let entropy = render.entropy(node);
        if entropy >= HOTSPOT_BITS {
            let mut next: Vec<&Node> = node
                .children
                .iter()
                .map(|&child| &cohort.nodes[child as usize])
                .collect();
            next.sort_by(|left, right| {
                right
                    .visits
                    .cmp(&left.visits)
                    .then_with(|| build.token(left.token).cmp(build.token(right.token)))
            });
            hotspots.push(HotspotSummary {
                path: render.path(id as u32),
                visits: u64::from(node.visits),
                entropy: stats::round4(entropy),
                next: next
                    .into_iter()
                    .take(3)
                    .map(|child| build.token(child.token).to_owned())
                    .collect(),
            });
        }
    }
    forks.sort_by(|left, right| {
        left.fork
            .p_value
            .total_cmp(&right.fork.p_value)
            .then(right.fork.gap.total_cmp(&left.fork.gap))
            .then_with(|| left.path.cmp(&right.path))
    });
    forks.truncate(SUMMARY_ROWS);
    hotspots.sort_by(|left, right| {
        (right.entropy * right.visits as f64)
            .total_cmp(&(left.entropy * left.visits as f64))
            .then_with(|| left.path.cmp(&right.path))
    });
    hotspots.truncate(SUMMARY_ROWS);
    let root = render.node(0, &shown);
    FateMap {
        tenant: cohort.tenant.clone(),
        agent: cohort.agent.clone(),
        task: request.task.clone(),
        runs: summary.runs,
        success: summary.success,
        failure: summary.failure,
        abandoned: summary.abandoned,
        unknown: summary.unknown,
        base_rate: summary.base_rate,
        truncated: build.bounds.truncated(),
        bounds: build.bounds.clone(),
        window: build.window,
        built_at_ns: build.built_at_ns,
        depth: request.depth,
        min_support: request.min_support,
        nodes_total: cohort.nodes.len() as u64,
        nodes_shown: shown.len() as u64,
        fatal_forks: forks,
        hotspots,
        calibration: cohort.calibration.clone(),
        root,
    }
}

// ------------------------------------------------------------- foresight

/// A run as foresight reads it.
struct RunPath {
    agent: String,
    groups: Vec<(String, usize)>,
    complete: bool,
    steps: u64,
    outcome: Outcome,
}

/// The behaviour of a run's stored spans, read the way a build reads them.
fn run_path(spans: &[Span], truncated: bool, now_ns: u64) -> RunPath {
    let mut steps: Vec<(OrderKey, String, Option<String>)> = Vec::new();
    let mut earliest: Option<(OrderKey, &str)> = None;
    for span in spans {
        let key = order_key(span);
        if earliest.map_or(true, |(first, _)| key < first) {
            earliest = Some((key, span.service.as_str()));
        }
        let (kind, signature, agent) =
            semconv::step_signature_with(span, &semconv::facts(&span.attributes));
        if kind.is_step() {
            steps.push((key, stem(&signature), agent));
        }
    }
    steps.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let agent = steps
        .first()
        .and_then(|(_, _, agent)| agent.clone())
        .or_else(|| earliest.map(|(_, service)| service.to_owned()))
        .unwrap_or_default();
    let total = steps.len();
    let complete = !truncated && total <= RUN_STEP_WINDOW;
    steps.truncate(RUN_STEP_WINDOW);
    let mut outcome =
        attribution::session_outcome(spans, now_ns, crate::mcp::SESSION_IDLE_NS).outcome;
    if truncated {
        outcome = Outcome::Unknown;
    }
    RunPath {
        agent: stem(&agent),
        groups: groups(steps.into_iter().map(|(_, signature, _)| signature)),
        complete,
        steps: total as u64,
        outcome,
    }
}

/// A path walked down a cohort's tree, with one run left out.
struct Walked {
    /// Node ids matched, the root first.
    matched: Vec<u32>,
    /// The token the walk could not follow, and whether that is because
    /// the node bound kept the branch from being built.
    stopped: Option<(String, bool)>,
}

fn walk(
    build: &Build,
    cohort: &Cohort,
    path: &[String],
    left_out: Option<(&[u32], Outcome)>,
) -> Walked {
    let counts = |id: u32| -> Counts {
        let counts = cohort.nodes[id as usize].counts();
        match left_out {
            Some((chain, outcome)) if chain.contains(&id) => counts.without(outcome),
            _ => counts,
        }
    };
    let mut matched = vec![0_u32];
    for token in path {
        let at = *matched.last().unwrap_or(&0);
        let node = &cohort.nodes[at as usize];
        let next = build.tokens.get(token).and_then(|token_id| {
            node.children
                .iter()
                .copied()
                .find(|&child| cohort.nodes[child as usize].token == token_id)
        });
        match next.filter(|&child| counts(child).visits > 0) {
            Some(child) => matched.push(child),
            // A path read only in part ends in the fold token: what follows
            // is unknown, not new, so the walk ends where the reading did.
            None if token == FOLD_TOKEN => break,
            None => {
                return Walked {
                    matched,
                    stopped: Some((token.clone(), node.cut > 0)),
                }
            }
        }
    }
    Walked {
        matched,
        stopped: None,
    }
}

/// Everything a verdict is made of, for the route and the fuse alike.
struct Judged {
    verdict: ForesightVerdict,
    explanation: String,
    node: Option<(u32, Counts)>,
    base: Counts,
    divergence: Option<Divergence>,
    next: Vec<NextOption>,
    recommended: Option<String>,
    matched_depth: usize,
}

/// A dollar amount to the micro-dollar: summing thousands of metered costs
/// leaves float noise in the last digits that is not money.
fn round_usd(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn percent(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn judge(
    build: &Build,
    cohort: &Cohort,
    path: &[String],
    left_out: Option<(&[u32], Outcome)>,
    min_support: u64,
) -> Judged {
    let counts = |id: u32| -> Counts {
        let counts = cohort.nodes[id as usize].counts();
        match left_out {
            Some((chain, outcome)) if chain.contains(&id) => counts.without(outcome),
            _ => counts,
        }
    };
    let base = counts(0);
    let walked = walk(build, cohort, path, left_out);
    let deepest = *walked.matched.last().unwrap_or(&0);
    let here = counts(deepest);
    let matched_depth = walked.matched.len() - 1;
    let mut children: Vec<(u32, Counts)> = cohort.nodes[deepest as usize]
        .children
        .iter()
        .map(|&child| (child, counts(child)))
        .filter(|(_, counts)| counts.visits > 0)
        .collect();
    children.sort_by(|left, right| {
        right.1.visits.cmp(&left.1.visits).then_with(|| {
            build
                .token(cohort.nodes[left.0 as usize].token)
                .cmp(build.token(cohort.nodes[right.0 as usize].token))
        })
    });
    let token_of = |id: u32| build.token(cohort.nodes[id as usize].token).to_owned();
    let next: Vec<NextOption> = children
        .iter()
        .filter(|(_, counts)| counts.visits >= DEFAULT_MIN_SUPPORT)
        .take(NEXT_OPTIONS)
        .map(|(id, counts)| {
            let (low, high) = stats::wilson(counts.success, counts.visits);
            NextOption {
                token: token_of(*id),
                visits: counts.visits,
                success_rate: stats::round4(counts.rate().unwrap_or(0.0)),
                ci95: [stats::round4(low), stats::round4(high)],
            }
        })
        .collect();
    let recommended = children
        .iter()
        .filter(|(_, counts)| counts.visits >= min_support)
        .map(|(id, counts)| (*id, stats::wilson(counts.success, counts.visits).0))
        .filter(|(_, low)| here.rate().is_some_and(|rate| *low > rate))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(id, _)| token_of(id));
    let mut judged = Judged {
        verdict: ForesightVerdict::InsufficientHistory,
        explanation: String::new(),
        node: Some((deepest, here)),
        base,
        divergence: None,
        next,
        recommended,
        matched_depth,
    };
    if base.visits == 0 {
        judged.node = None;
        judged.explanation = format!(
            "no completed runs of agent `{}` in the window to compare with",
            cohort.agent
        );
        return judged;
    }
    if let Some((took, cut)) = walked.stopped {
        if cut {
            judged.explanation = format!(
                "the map was cut short after {matched_depth} tokens of this path by its node \
                 bound (--fate-max-nodes), so `{took}` cannot be judged"
            );
            return judged;
        }
        let common: Vec<String> = children
            .iter()
            .take(3)
            .map(|(id, _)| token_of(*id))
            .collect();
        judged.verdict = ForesightVerdict::Novel;
        judged.explanation = if common.is_empty() {
            format!(
                "no earlier run of this agent went on past this point ({} runs ended here); \
                 this one did, with `{took}`",
                here.visits
            )
        } else {
            format!(
                "no earlier run of this agent took `{took}` at this point; the {} runs that \
                 got here went on with {}",
                here.visits,
                common
                    .iter()
                    .map(|token| format!("`{token}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        judged.divergence = Some(Divergence {
            depth: matched_depth,
            common_next: common,
            took,
        });
        return judged;
    }
    let base_rate = base.rate().unwrap_or(0.0);
    if here.visits < min_support {
        judged.explanation = format!(
            "only {} earlier runs took this path; a verdict needs at least {min_support} \
             (runs that did succeeded {} of {} times)",
            here.visits, here.success, here.visits
        );
        return judged;
    }
    let (low, high) = stats::wilson(here.success, here.visits);
    let rate = here.rate().unwrap_or(0.0);
    let subject = if matched_depth == 0 {
        "runs of this agent (this one has taken no steps yet)"
    } else {
        "runs that took this path"
    };
    judged.verdict = if high < 0.5 {
        ForesightVerdict::LikelyFailure
    } else if high < base_rate {
        ForesightVerdict::AtRisk
    } else {
        ForesightVerdict::OnTrack
    };
    judged.explanation = format!(
        "{subject} succeeded {} of {} times ({}, 95% CI {}–{}); the agent's overall success \
         rate is {:.0}%",
        here.success,
        here.visits,
        percent(rate),
        percent(low).trim_end_matches('%'),
        percent(high),
        base_rate * 100.0
    );
    judged
}

impl Store {
    /// Cohorts in `scope` (a tenant, or `None` for every tenant) over
    /// `window` (default: the last thirty days, as of `now_ns`).
    pub fn fate_cohorts(
        &self,
        scope: Option<&str>,
        window: Option<(u64, u64)>,
        now_ns: u64,
    ) -> Result<Cohorts> {
        validate_window(window)?;
        let deadline = self.query_deadline(Instant::now());
        let key = BuildKey::of(scope, None, window);
        let build = self.fate.get_or_build(self, &key, now_ns, deadline)?;
        let mut cohorts = Vec::new();
        for cohort in &build.cohorts {
            cohorts.push(cohort.summary(None));
            cohorts.extend(cohort.tasks.iter().cloned());
        }
        Ok(Cohorts {
            cohorts,
            window: build.window,
            built_at_ns: build.built_at_ns,
            truncated: build.bounds.truncated(),
            bounds: build.bounds.clone(),
        })
    }

    /// The Fate Map of one agent in `tenant`, or `None` when the agent has
    /// no runs in the window.
    pub fn fate_map(
        &self,
        tenant: &str,
        request: &MapRequest,
        now_ns: u64,
    ) -> Result<Option<FateMap>> {
        request.validate()?;
        let deadline = self.query_deadline(Instant::now());
        let key = BuildKey::of(Some(tenant), request.task.as_deref(), request.window);
        let build = self.fate.get_or_build(self, &key, now_ns, deadline)?;
        Ok(build
            .cohort(tenant, &request.agent)
            .map(|cohort| render_map(&build, cohort, request)))
    }

    /// Foresight for one run of `tenant`, against its agent's map over the
    /// last thirty days, or `None` when no span carries the run.
    /// `min_support` is the support a verdict needs.
    pub fn foresee(
        &self,
        tenant: &str,
        run: &str,
        min_support: u64,
        now_ns: u64,
    ) -> Result<Option<Foresight>> {
        validate_run(run)?;
        let min_support = min_support.max(1);
        let deadline = self.query_deadline(Instant::now());
        let read = self.run_spans(Some(tenant), run, FORESIGHT_RUN_SPANS, deadline)?;
        if read.spans.is_empty() {
            return Ok(None);
        }
        let key = BuildKey::of(Some(tenant), None, None);
        let build = self.fate.get_or_build(self, &key, now_ns, deadline)?;
        let read_path = run_path(&read.spans, read.truncated, now_ns);
        let path = path_of(&read_path.groups, read_path.complete);
        let mut foresight = Foresight {
            run: run.to_owned(),
            tenant: tenant.to_owned(),
            agent: read_path.agent.clone(),
            path: path.clone(),
            steps: read_path.steps,
            matched_depth: 0,
            node: None,
            p_success: None,
            ci95: None,
            support: 0,
            base_rate: None,
            verdict: ForesightVerdict::InsufficientHistory,
            explanation: String::new(),
            divergence: None,
            next: Vec::new(),
            recommended_next: None,
            outcome: read_path.outcome,
            cohort_runs: 0,
            excluded_self: false,
            partial: read.truncated,
            min_support,
            map_built_at_ns: Some(build.built_at_ns),
        };
        let Some(cohort) = build.cohort(tenant, &read_path.agent) else {
            foresight.explanation = format!(
                "no completed runs of agent `{}` in the window to compare with",
                read_path.agent
            );
            return Ok(Some(foresight));
        };
        let member = cohort.members.get(run).copied();
        let chain = member.map(|member| cohort.chain(member.deepest));
        let left_out = match (&chain, member) {
            (Some(chain), Some(member)) => Some((chain.as_slice(), member.outcome)),
            _ => None,
        };
        foresight.excluded_self = left_out.is_some();
        let judged = judge(&build, cohort, &path, left_out, min_support);
        fill(&mut foresight, &build, cohort, &judged);
        Ok(Some(foresight))
    }

    /// Builds, off any request path, the maps the predictive fuses asked
    /// for and found missing or stale — at most two per call, each under
    /// the query deadline. Returns how many it built. `traza-server` calls
    /// it every few seconds; an embedder that sets `min_success_odds` on a
    /// policy calls it on its own cadence, or its fuses see no map.
    pub fn maintain_fate(&self) -> Result<usize> {
        let mut built = 0;
        let mut first_error = None;
        for key in self.fate.take_demand(WARM_PER_CALL) {
            let deadline = self.query_deadline(Instant::now());
            match self
                .fate
                .get_or_build(self, &key, crate::mcp::unix_nanos_now(), deadline)
            {
                Ok(_) => built += 1,
                Err(error) => {
                    self.fate.failed(&key);
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(built),
        }
    }

    /// Foresight for one run of `tenant` read from a CACHED map only — the
    /// answer a predictive fuse acts on, for a surface that must never build
    /// a map on its request path (a live-runs listing, say). Reads the run's
    /// spans under the query deadline; with no map cached, or a run that
    /// cannot be read in time, the verdict is `insufficient_history` and a
    /// background build is requested ([`Self::maintain_fate`]).
    pub fn foresee_cached(&self, tenant: &str, run: &str, min_support: u64) -> GuardForesight {
        self.fate_for_guard(tenant, run, min_support.max(1), None, Instant::now())
    }

    /// Drops every cached map: called by every erasure.
    pub(crate) fn forget_fate(&self) {
        self.fate.invalidate();
    }

    /// Foresight for a fuse verdict: read only from a cached map, never
    /// building one. `started` anchors the request's one budget.
    fn fate_for_guard(
        &self,
        tenant: &str,
        run: &str,
        min_support: u64,
        next_tool: Option<&str>,
        started: Instant,
    ) -> GuardForesight {
        let mut out = GuardForesight {
            verdict: ForesightVerdict::InsufficientHistory,
            p_success: None,
            ci95: None,
            support: 0,
            successes: 0,
            base_rate: None,
            matched_depth: 0,
            next_tool: None,
            raised: false,
            explanation: String::new(),
            map_built_at_ns: None,
        };
        let key = BuildKey::of(Some(tenant), None, None);
        let Some(build) = self.fate.peek(&key) else {
            out.explanation = "no Fate Map is cached for this tenant yet; it is being built in \
                               the background"
                .to_owned();
            return out;
        };
        out.map_built_at_ns = Some(build.built_at_ns);
        let deadline = self.query_deadline(started);
        let read = match self.run_spans(Some(tenant), run, FORESIGHT_RUN_SPANS, deadline) {
            Ok(read) => read,
            Err(error) => {
                out.explanation = format!("the run could not be read for foresight: {error}");
                return out;
            }
        };
        if read.spans.is_empty() {
            out.explanation = "the run has no recorded steps yet".to_owned();
            return out;
        }
        let read_path = run_path(&read.spans, read.truncated, crate::mcp::unix_nanos_now());
        let Some(cohort) = build.cohort(tenant, &read_path.agent) else {
            out.explanation = format!(
                "no completed runs of agent `{}` in the window to compare with",
                read_path.agent
            );
            return out;
        };
        let member = cohort.members.get(run).copied();
        let chain = member.map(|member| cohort.chain(member.deepest));
        let left_out = match (&chain, member) {
            (Some(chain), Some(member)) => Some((chain.as_slice(), member.outcome)),
            _ => None,
        };
        let current = path_of(&read_path.groups, read_path.complete);
        let mut judged = judge(&build, cohort, &current, left_out, min_support);
        // The step about to be taken, when the check names its tool: judged
        // with it appended, if runs that took it are known; otherwise it adds
        // no evidence and the path so far is what is judged.
        if let Some(tool) = next_tool.filter(|_| read_path.complete) {
            let mut extended = read_path.groups.clone();
            let next = stem(&format!("tool:{tool}"));
            match extended.last_mut() {
                Some((last, count)) if *last == next => *count += 1,
                _ => extended.push((next, 1)),
            }
            let hypothetical = judge(
                &build,
                cohort,
                &path_of(&extended, true),
                left_out,
                min_support,
            );
            if matches!(
                hypothetical.verdict,
                ForesightVerdict::OnTrack
                    | ForesightVerdict::AtRisk
                    | ForesightVerdict::LikelyFailure
            ) {
                judged = hypothetical;
                judged.explanation =
                    format!("if it calls tool:{tool} next: {}", judged.explanation);
                out.next_tool = Some(tool.to_owned());
            }
        }
        out.verdict = judged.verdict;
        out.explanation = judged.explanation.clone();
        out.matched_depth = judged.matched_depth;
        out.base_rate = judged.base.rate().map(stats::round4);
        if let Some((_, here)) = judged.node {
            out.support = here.visits;
            out.successes = here.success;
            out.p_success = here.rate().map(stats::round4);
            let (low, high) = stats::wilson(here.success, here.visits);
            out.ci95 = (here.visits > 0).then(|| [stats::round4(low), stats::round4(high)]);
        }
        out
    }
}

/// Copies a judgement into a foresight document.
fn fill(foresight: &mut Foresight, build: &Build, cohort: &Cohort, judged: &Judged) {
    foresight.verdict = judged.verdict;
    foresight.explanation = judged.explanation.clone();
    foresight.matched_depth = judged.matched_depth;
    foresight.divergence = judged.divergence.clone();
    foresight.next = judged.next.clone();
    foresight.recommended_next = judged.recommended.clone();
    foresight.cohort_runs = judged.base.visits;
    let base_rate = judged.base.rate();
    foresight.base_rate = base_rate.map(stats::round4);
    if let Some((id, here)) = judged.node {
        let node = &cohort.nodes[id as usize];
        let (low, high) = stats::wilson(here.success, here.visits);
        let rate = here.rate();
        foresight.support = here.visits;
        foresight.p_success = rate.map(stats::round4);
        foresight.ci95 = (here.visits > 0).then(|| [stats::round4(low), stats::round4(high)]);
        // The node's cost is summed over every run through it; the run
        // itself is not taken out of it (its share is one run's cost).
        foresight.node = Some(NodeStats {
            token: build.token(node.token).to_owned(),
            depth: usize::from(node.depth),
            visits: here.visits,
            success: here.success,
            failure: here.failure,
            abandoned: here.abandoned,
            success_rate: rate.map(stats::round4),
            ci95: [stats::round4(low), stats::round4(high)],
            p_fail_given_here: (here.visits > 0)
                .then(|| stats::round4(here.failure as f64 / here.visits as f64)),
            lift: match (rate, base_rate) {
                (Some(rate), Some(base)) if base > 0.0 => Some(stats::round4(rate / base)),
                _ => None,
            },
            cost_usd_avg: (node.visits > 0)
                .then(|| round_usd(node.cost_usd_sum / f64::from(node.visits))),
        });
    }
}

fn validate_run(run: &str) -> Result<()> {
    if run.is_empty() {
        return Err(Error::Invalid("run id is empty".to_owned()));
    }
    if run.len() > MAX_RUN_ID_BYTES {
        return Err(Error::Invalid(format!(
            "run id is longer than {MAX_RUN_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Applies a policy's predictive limits to a verdict document: called by
/// the fuse check and verdict queries — never by ingest — once the usage
/// verdict is made and no guard lock is held. A run already stopped is not
/// foreseen. The foresight stop is not latched: it is a prediction, made
/// again at every question, and it lifts if the run's path moves to a node
/// whose odds clear the limit.
pub(crate) fn apply_to_verdict(
    store: &Store,
    doc: &mut VerdictDoc,
    action: Action,
    next: Option<&NextStep>,
    started: Instant,
) {
    let Some(odds) = doc.limits.min_success_odds else {
        return;
    };
    if doc.verdict == Verdict::Stop {
        return;
    }
    let min_support = doc
        .limits
        .foresight_min_support
        .unwrap_or(DEFAULT_VERDICT_SUPPORT);
    let after_steps = doc
        .limits
        .foresight_after_steps
        .unwrap_or(DEFAULT_AFTER_STEPS);
    let next_tool = next.and_then(|next| next.tool.as_deref());
    let mut foresight =
        store.fate_for_guard(&doc.tenant, &doc.run, min_support, next_tool, started);
    let judged = matches!(
        foresight.verdict,
        ForesightVerdict::OnTrack | ForesightVerdict::AtRisk | ForesightVerdict::LikelyFailure
    );
    let upper = foresight.ci95.map_or(1.0, |ci| ci[1]);
    if judged && foresight.support >= min_support && doc.usage.steps >= after_steps && upper < odds
    {
        foresight.raised = true;
        let path = match &foresight.next_tool {
            Some(tool) => format!("this path and then called tool:{tool}"),
            None => "this path".to_owned(),
        };
        doc.reasons.push(format!(
            "foresight: runs that took {path} succeeded {} of {} times (≤{})",
            foresight.successes,
            foresight.support,
            percent(upper)
        ));
        doc.verdict = match action {
            Action::Stop => Verdict::Stop,
            Action::Warn => doc.verdict.max(Verdict::Warn),
        };
    }
    doc.foresight = Some(foresight);
}

#[cfg(test)]
mod tests;
