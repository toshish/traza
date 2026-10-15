//! Run diffing: two runs aligned step by step, and the first place they part.
//!
//! "Why did this run fail when yesterday's succeeded?" is answered by putting
//! the two tapes side by side. Steps are aligned on their SIGNATURE — the
//! fingerprint without its request hash, `kind:name` — by longest common
//! subsequence, so a run that inserted one extra tool call still lines up
//! with the run that did not, rather than every later step reading as changed.
//!
//! Each aligned pair is then classified by what differs:
//!
//! | Pair | `op` | `why` |
//! |---|---|---|
//! | same signature, same request digest, same response digest, same config | `same` | — |
//! | same signature, different request digest | `changed` | `input` |
//! | same signature and request, different response digest | `changed` | `decision` |
//! | llm steps whose model or `gen_ai.request.*` parameters differ | `changed` | `config` |
//! | a step only one run has | `insert` / `delete` | `structure` |
//!
//! A model change alters the signature (`llm:gpt-4o` is not
//! `llm:gpt-4o-mini`), so the alignment alone would report it as a deletion
//! plus an insertion. Inside each unaligned gap, llm steps from the two runs
//! that both name a model are therefore paired in order and reported as one
//! `config` change — which is what a model swap is.
//!
//! The first non-`same` entry is the run pair's FIRST DIVERGENCE, with a
//! sentence saying what differed. Everything is computed from one read of each
//! run under one compute budget.

use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::attribution;
use crate::semconv::StepKind;
use crate::tape::{RunTape, TapeStep};
use crate::{Result, Span, Store};

/// Steps compared per side. Past it the side is truncated to its earliest
/// steps and the diff says so.
pub const MAX_DIFF_STEPS: usize = 5000;

/// Spans read per side. Past it the side is read from its earliest spans:
/// its steps, and every summary over its spans (cost, tokens, duration,
/// errors), describe that beginning, and its outcome is `unknown`.
pub const MAX_DIFF_SPANS: usize = 50_000;

/// Below this many cells the alignment is a plain dynamic-programming table;
/// above it, Hirschberg's linear-space recursion, so two 5,000-step runs cost
/// tens of kilobytes of table rather than a hundred megabytes.
const DIRECT_TABLE_CELLS: usize = 1 << 16;

/// Hex characters of a digest quoted in an explanation.
const DIGEST_PREVIEW: usize = 12;

/// Characters of one producer-chosen value quoted in an explanation — a
/// model, tool or span name, a parameter's key or its value. Producers bound
/// none of these, and an explanation travels into size-capped results (the
/// MCP ceiling), so each is clipped where it is quoted.
const QUOTED_CHARS: usize = 80;

/// Characters of a whole explanation, at most.
const MAX_EXPLANATION_CHARS: usize = 400;

/// One side of a diff, summarised.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunSide {
    /// The run id.
    pub run: String,
    /// Steps compared (llm, tool and agent spans).
    pub steps: usize,
    /// Cost in USD over every span of the run, metered or priced.
    pub cost_usd: f64,
    /// Tokens over every span of the run ([`crate::semconv::LlmFacts::total`]).
    pub tokens: u64,
    /// Latest end minus earliest start across the run's spans.
    pub duration_ns: u64,
    /// Spans whose status is `error`.
    pub errors: usize,
    /// How the run ended ([`attribution::session_outcome`]) — `unknown`
    /// when the side was truncated, because a run read from its beginning
    /// has not been read to its end.
    pub outcome: attribution::Outcome,
    /// Whether this side had more than [`MAX_DIFF_SPANS`] spans, so that
    /// every figure above describes its earliest spans only.
    pub truncated: bool,
}

/// What an aligned entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// The same call with the same input and output.
    Same,
    /// The same call position, differing in input, decision or config.
    Changed,
    /// A step only run `b` has.
    Insert,
    /// A step only run `a` has.
    Delete,
}

/// What made two runs differ at one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DivergenceKind {
    /// The step received different input.
    Input,
    /// The step received the same input and produced different output.
    Decision,
    /// The model call ran with a different model or parameters.
    Config,
    /// One run has a step the other does not.
    Structure,
}

impl DivergenceKind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Decision => "decision",
            Self::Config => "config",
            Self::Structure => "structure",
        }
    }
}

/// One entry of the alignment.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Aligned {
    /// What the entry is.
    pub op: Op,
    /// The step's index on run `a`'s tape, `None` for an insertion.
    pub a: Option<usize>,
    /// The step's index on run `b`'s tape, `None` for a deletion.
    pub b: Option<usize>,
    /// Run `a`'s step fingerprint.
    pub fingerprint_a: Option<String>,
    /// Run `b`'s step fingerprint.
    pub fingerprint_b: Option<String>,
    /// Why the entry is not `same`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<DivergenceKind>,
}

/// Where two runs first part.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FirstDivergence {
    /// Run `a`'s step index. For an insertion, the index of `a`'s next step —
    /// equal to its step count when the insertion is past its end.
    pub a: usize,
    /// Run `b`'s step index, under the same rule for a deletion.
    pub b: usize,
    /// What differed.
    pub kind: DivergenceKind,
    /// One sentence a reader can check against the two tapes. It quotes tool,
    /// model and span names, which are producer text.
    pub explanation: String,
}

/// `b` minus `a`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Delta {
    /// Cost difference in USD.
    pub cost_usd: f64,
    /// Token difference.
    pub tokens: i64,
    /// Duration difference.
    pub duration_ns: i64,
    /// Error-count difference.
    pub errors: i64,
    /// Step-count difference.
    pub steps: i64,
}

/// Two runs compared.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunDiff {
    /// Run `a`, summarised.
    pub a: RunSide,
    /// Run `b`, summarised.
    pub b: RunSide,
    /// The alignment, in order.
    pub aligned: Vec<Aligned>,
    /// The first entry that is not `same`, or `None` when the runs match step
    /// for step.
    pub first_divergence: Option<FirstDivergence>,
    /// `b` minus `a`.
    pub delta: Delta,
    /// Whether either side was cut to [`MAX_DIFF_STEPS`] steps or
    /// [`MAX_DIFF_SPANS`] spans, in which case the alignment describes
    /// prefixes and its tail may report steps as missing that the cut removed.
    /// Which side's summary is also a prefix is its own `truncated`.
    pub truncated: bool,
}

/// A diff refused because one of its runs has no spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingRun(pub String);

impl Store {
    /// Compares run `a` with run `b` under `tenant` (the scoping rules of
    /// [`Store::get_run_in`]). `now_ns` dates the outcome rule — a run whose
    /// last span ended inside the idle window is still `unknown`.
    ///
    /// Both runs are read under ONE compute budget. A run with no spans is
    /// [`MissingRun`], naming it.
    pub fn diff_runs(
        &self,
        tenant: Option<&str>,
        a: &str,
        b: &str,
        now_ns: u64,
    ) -> Result<std::result::Result<RunDiff, MissingRun>> {
        self.diff_runs_capped(tenant, a, b, now_ns, MAX_DIFF_SPANS, MAX_DIFF_STEPS)
    }

    /// [`Self::diff_runs`] with explicit per-side caps, so tests can reach
    /// the truncation edges without fifty thousand spans.
    pub(crate) fn diff_runs_capped(
        &self,
        tenant: Option<&str>,
        a: &str,
        b: &str,
        now_ns: u64,
        max_spans: usize,
        max_steps: usize,
    ) -> Result<std::result::Result<RunDiff, MissingRun>> {
        let deadline = self.query_deadline(Instant::now());
        let left = self.run_tape(tenant, a, max_spans, max_steps, deadline)?;
        if left.spans.is_empty() {
            return Ok(Err(MissingRun(a.to_owned())));
        }
        let right = self.run_tape(tenant, b, max_spans, max_steps, deadline)?;
        if right.spans.is_empty() {
            return Ok(Err(MissingRun(b.to_owned())));
        }
        let side_a = self.side(a, &left, now_ns);
        let side_b = self.side(b, &right, now_ns);
        let aligned = align(&left.steps, &right.steps);
        let first_divergence = first_divergence(&aligned, &left.steps, &right.steps);
        let delta = Delta {
            cost_usd: side_b.cost_usd - side_a.cost_usd,
            tokens: signed(side_b.tokens) - signed(side_a.tokens),
            duration_ns: signed(side_b.duration_ns) - signed(side_a.duration_ns),
            errors: signed(side_b.errors as u64) - signed(side_a.errors as u64),
            steps: signed(side_b.steps as u64) - signed(side_a.steps as u64),
        };
        Ok(Ok(RunDiff {
            a: side_a,
            b: side_b,
            aligned,
            first_divergence,
            delta,
            truncated: left.truncated || right.truncated,
        }))
    }

    fn side(&self, run: &str, tape: &RunTape, now_ns: u64) -> RunSide {
        let (cost_usd, tokens) = run_cost(self, &tape.spans);
        let ended = attribution::session_outcome(&tape.spans, now_ns, crate::mcp::SESSION_IDLE_NS);
        RunSide {
            run: run.to_owned(),
            steps: tape.steps.len(),
            cost_usd,
            tokens,
            duration_ns: run_duration(&tape.spans),
            errors: ended.error_count,
            outcome: match tape.spans_truncated {
                true => attribution::Outcome::Unknown,
                false => ended.outcome,
            },
            truncated: tape.spans_truncated,
        }
    }
}

/// Priced cost and total tokens over every span of a run.
pub(crate) fn run_cost(store: &Store, spans: &[Span]) -> (f64, u64) {
    spans.iter().fold((0.0, 0), |(cost, tokens), span| {
        let facts = store.facts(span);
        (
            cost + facts.cost_usd.unwrap_or(0.0),
            tokens.saturating_add(facts.total()),
        )
    })
}

/// Latest end minus earliest start, zero for an empty run.
pub(crate) fn run_duration(spans: &[Span]) -> u64 {
    let start = spans.iter().map(|span| span.start_time_ns).min();
    let end = spans.iter().map(|span| span.end_time_ns).max();
    match (start, end) {
        (Some(start), Some(end)) => end.saturating_sub(start),
        _ => 0,
    }
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Aligns two tapes and classifies every entry.
fn align(a: &[TapeStep], b: &[TapeStep]) -> Vec<Aligned> {
    let left: Vec<&str> = a
        .iter()
        .map(|step| step.detail.signature.as_str())
        .collect();
    let right: Vec<&str> = b
        .iter()
        .map(|step| step.detail.signature.as_str())
        .collect();
    let pairs = lcs_pairs(&left, &right);
    let mut aligned = Vec::with_capacity(a.len().max(b.len()));
    let (mut next_a, mut next_b) = (0, 0);
    for (pa, pb) in pairs {
        gap(a, b, next_a..pa, next_b..pb, &mut aligned);
        let why = classify(&a[pa], &b[pb]);
        aligned.push(Aligned {
            op: if why.is_some() { Op::Changed } else { Op::Same },
            a: Some(pa),
            b: Some(pb),
            fingerprint_a: Some(a[pa].fingerprint.clone()),
            fingerprint_b: Some(b[pb].fingerprint.clone()),
            why,
        });
        next_a = pa + 1;
        next_b = pb + 1;
    }
    gap(a, b, next_a..a.len(), next_b..b.len(), &mut aligned);
    aligned
}

/// Why two steps with the same signature differ, or `None` when they do not.
fn classify(a: &TapeStep, b: &TapeStep) -> Option<DivergenceKind> {
    if a.kind == StepKind::Llm && a.detail.config != b.detail.config {
        return Some(DivergenceKind::Config);
    }
    if a.request_digest != b.request_digest {
        return Some(DivergenceKind::Input);
    }
    if a.response_digest != b.response_digest {
        return Some(DivergenceKind::Decision);
    }
    None
}

/// Whether two unaligned steps are one model call whose model changed.
fn model_swap(a: &TapeStep, b: &TapeStep) -> bool {
    a.kind == StepKind::Llm
        && b.kind == StepKind::Llm
        && a.model.is_some()
        && b.model.is_some()
        && a.model != b.model
}

/// The unaligned steps between two aligned pairs (or past the last one): llm
/// steps that both name a model are paired in order as `config` changes, and
/// everything else is a deletion from `a` or an insertion from `b`, in tape
/// order.
fn gap(
    a: &[TapeStep],
    b: &[TapeStep],
    left: std::ops::Range<usize>,
    right: std::ops::Range<usize>,
    out: &mut Vec<Aligned>,
) {
    let (mut i, mut j) = (left.start, right.start);
    while i < left.end || j < right.end {
        if i < left.end && j < right.end && model_swap(&a[i], &b[j]) {
            out.push(Aligned {
                op: Op::Changed,
                a: Some(i),
                b: Some(j),
                fingerprint_a: Some(a[i].fingerprint.clone()),
                fingerprint_b: Some(b[j].fingerprint.clone()),
                why: Some(DivergenceKind::Config),
            });
            i += 1;
            j += 1;
            continue;
        }
        // A step of `a` that could still pair with a later model call of `b`
        // waits while `b`'s non-pairing steps are inserted, and vice versa,
        // so a swap is not split by an unrelated step beside it.
        let a_waits = i < left.end
            && a[i].kind == StepKind::Llm
            && a[i].model.is_some()
            && j < right.end
            && !(b[j].kind == StepKind::Llm && b[j].model.is_some());
        if i < left.end && !a_waits {
            out.push(Aligned {
                op: Op::Delete,
                a: Some(i),
                b: None,
                fingerprint_a: Some(a[i].fingerprint.clone()),
                fingerprint_b: None,
                why: Some(DivergenceKind::Structure),
            });
            i += 1;
        } else {
            out.push(Aligned {
                op: Op::Insert,
                a: None,
                b: Some(j),
                fingerprint_a: None,
                fingerprint_b: Some(b[j].fingerprint.clone()),
                why: Some(DivergenceKind::Structure),
            });
            j += 1;
        }
    }
}

/// The first entry that is not `same`, explained.
fn first_divergence(
    aligned: &[Aligned],
    a: &[TapeStep],
    b: &[TapeStep],
) -> Option<FirstDivergence> {
    // The next index on each side, for an entry the side does not have.
    let (mut next_a, mut next_b) = (0, 0);
    for entry in aligned {
        if entry.op != Op::Same {
            let kind = entry.why.unwrap_or(DivergenceKind::Structure);
            let explanation = clip(&explain(entry, kind, a, b), MAX_EXPLANATION_CHARS);
            return Some(FirstDivergence {
                a: entry.a.unwrap_or(next_a),
                b: entry.b.unwrap_or(next_b),
                kind,
                explanation,
            });
        }
        next_a = entry.a.map_or(next_a, |index| index + 1);
        next_b = entry.b.map_or(next_b, |index| index + 1);
    }
    None
}

fn preview(digest: Option<&String>) -> String {
    digest.map_or_else(
        || "none".to_owned(),
        |digest| digest[..digest.len().min(DIGEST_PREVIEW)].to_owned(),
    )
}

/// `text`, cut to `max` characters with an ellipsis when longer.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

fn quote(text: &str) -> String {
    clip(text, QUOTED_CHARS)
}

fn explain(entry: &Aligned, kind: DivergenceKind, a: &[TapeStep], b: &[TapeStep]) -> String {
    match (entry.a.map(|i| &a[i]), entry.b.map(|j| &b[j])) {
        (Some(left), Some(right)) => {
            let at = format!(
                "step {} of a and step {} of b ({})",
                left.index,
                right.index,
                quote(&left.detail.signature)
            );
            match kind {
                DivergenceKind::Input => format!(
                    "{at} received different input: request {} in a, {} in b",
                    preview(left.request_digest.as_ref()),
                    preview(right.request_digest.as_ref()),
                ),
                DivergenceKind::Decision => format!(
                    "{at} received the same input and produced a different output: \
                     response {} in a, {} in b",
                    preview(left.response_digest.as_ref()),
                    preview(right.response_digest.as_ref()),
                ),
                DivergenceKind::Config if left.model != right.model => format!(
                    "step {} of a and step {} of b is the same model call with a different model: \
                     {} in a, {} in b",
                    left.index,
                    right.index,
                    quote(left.model.as_deref().unwrap_or("none")),
                    quote(right.model.as_deref().unwrap_or("none")),
                ),
                DivergenceKind::Config => {
                    let keys = config_differences(&left.detail.config, &right.detail.config);
                    format!("{at} ran with different parameters: {keys}")
                }
                DivergenceKind::Structure => format!("{at} differs in structure"),
            }
        }
        (Some(left), None) => format!(
            "run a has step {} ({}) where run b has no matching step",
            left.index,
            quote(&left.detail.signature)
        ),
        (None, Some(right)) => format!(
            "run b has step {} ({}) where run a has no matching step",
            right.index,
            quote(&right.detail.signature)
        ),
        (None, None) => "the runs differ".to_owned(),
    }
}

/// The configuration keys that differ, with both values, at most three.
fn config_differences(
    a: &serde_json::Map<String, Value>,
    b: &serde_json::Map<String, Value>,
) -> String {
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    let differing: Vec<String> = keys
        .into_iter()
        .filter(|key| a.get(*key) != b.get(*key))
        .map(|key| {
            let show = |value: Option<&Value>| {
                value.map_or_else(|| "unset".to_owned(), |value| quote(&value.to_string()))
            };
            format!(
                "{} {} in a, {} in b",
                quote(key),
                show(a.get(key)),
                show(b.get(key))
            )
        })
        .collect();
    let shown = differing.len().min(3);
    let mut text = differing[..shown].join("; ");
    if differing.len() > shown {
        text.push_str(&format!("; and {} more", differing.len() - shown));
    }
    text
}

// ---------------------------------------------------------------- alignment

/// The index pairs of a longest common subsequence of `a` and `b`, in order.
///
/// The common prefix and suffix are matched directly — two runs of the same
/// agent usually share most of both, which leaves only the middle for the
/// quadratic part — and the middle is aligned in linear space.
fn lcs_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix
        && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|index| (index, index)).collect();
    hirschberg(
        &a[prefix..a.len() - suffix],
        &b[prefix..b.len() - suffix],
        prefix,
        prefix,
        &mut pairs,
    );
    let (a_tail, b_tail) = (a.len() - suffix, b.len() - suffix);
    pairs.extend((0..suffix).map(|offset| (a_tail + offset, b_tail + offset)));
    pairs
}

/// Hirschberg's divide and conquer: split `a` in half, find where the two
/// halves' LCS lengths against `b` sum to the maximum, and recurse on each
/// side of that split. Linear space; ties take the earliest split, so the
/// alignment is deterministic.
fn hirschberg(
    a: &[&str],
    b: &[&str],
    a_offset: usize,
    b_offset: usize,
    out: &mut Vec<(usize, usize)>,
) {
    if a.is_empty() || b.is_empty() {
        return;
    }
    if a.len().saturating_mul(b.len()) <= DIRECT_TABLE_CELLS || a.len() == 1 {
        direct(a, b, a_offset, b_offset, out);
        return;
    }
    let middle = a.len() / 2;
    let forward = lcs_row(a[..middle].iter(), b.iter(), b.len());
    let backward = lcs_row(a[middle..].iter().rev(), b.iter().rev(), b.len());
    let split = (0..=b.len())
        .max_by(|&left, &right| {
            let score = |j: usize| forward[j] + backward[b.len() - j];
            // `max_by` keeps the LAST maximum; comparing in reverse on ties
            // makes it keep the first.
            score(left).cmp(&score(right)).then(right.cmp(&left))
        })
        .unwrap_or(0);
    hirschberg(&a[..middle], &b[..split], a_offset, b_offset, out);
    hirschberg(
        &a[middle..],
        &b[split..],
        a_offset + middle,
        b_offset + split,
        out,
    );
}

/// The last row of the LCS-length table: entry `j` is the LCS length of all
/// of `a` against the first `j` items of `b`.
fn lcs_row<'s, 'a: 's>(
    a: impl Iterator<Item = &'s &'a str>,
    b: impl Iterator<Item = &'s &'a str> + Clone,
    b_len: usize,
) -> Vec<u32> {
    let mut previous = vec![0u32; b_len + 1];
    let mut current = vec![0u32; b_len + 1];
    for left in a {
        for (j, right) in b.clone().enumerate() {
            current[j + 1] = if left == right {
                previous[j] + 1
            } else {
                current[j].max(previous[j + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous
}

/// The full table and its backtrack, for small subproblems.
fn direct(a: &[&str], b: &[&str], a_offset: usize, b_offset: usize, out: &mut Vec<(usize, usize)>) {
    let width = b.len() + 1;
    let mut table = vec![0u32; (a.len() + 1) * width];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            table[i * width + j] = if a[i - 1] == b[j - 1] {
                table[(i - 1) * width + j - 1] + 1
            } else {
                table[(i - 1) * width + j].max(table[i * width + j - 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            pairs.push((a_offset + i - 1, b_offset + j - 1));
            i -= 1;
            j -= 1;
        } else if table[(i - 1) * width + j] >= table[i * width + j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    out.extend(pairs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::{span, TempStore};
    use serde_json::json;

    /// Brute-force LCS length, the oracle for the linear-space alignment.
    fn lcs_length(a: &[&str], b: &[&str]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                table[i][j] = if a[i - 1] == b[j - 1] {
                    table[i - 1][j - 1] + 1
                } else {
                    table[i - 1][j].max(table[i][j - 1])
                };
            }
        }
        table[a.len()][b.len()]
    }

    fn assert_common_subsequence(a: &[&str], b: &[&str], pairs: &[(usize, usize)]) {
        for window in pairs.windows(2) {
            assert!(window[0].0 < window[1].0 && window[0].1 < window[1].1);
        }
        for &(i, j) in pairs {
            assert_eq!(a[i], b[j]);
        }
        assert_eq!(pairs.len(), lcs_length(a, b), "not a LONGEST subsequence");
    }

    #[test]
    fn alignment_is_a_longest_common_subsequence_at_every_size() {
        let alphabet = ["llm:m", "tool:a", "tool:b", "agent:x", "tool:c"];
        // A deterministic pseudo-random walk, so no dependency and no flake.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |modulus: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % modulus as u64) as usize
        };
        for (left, right) in [(0, 5), (7, 0), (40, 37), (300, 280), (600, 650)] {
            let a: Vec<&str> = (0..left).map(|_| alphabet[next(alphabet.len())]).collect();
            let b: Vec<&str> = (0..right).map(|_| alphabet[next(alphabet.len())]).collect();
            let pairs = lcs_pairs(&a, &b);
            assert_common_subsequence(&a, &b, &pairs);
        }
        // Large enough to take the Hirschberg path, with a shared prefix and
        // suffix around a differing middle.
        let mut a = vec!["llm:m"; 2000];
        let mut b = vec!["llm:m"; 2000];
        for index in 500..1500 {
            a[index] = alphabet[next(alphabet.len())];
            b[index] = alphabet[next(alphabet.len())];
        }
        let pairs = lcs_pairs(&a, &b);
        assert_common_subsequence(&a, &b, &pairs);
    }

    fn step_span(trace: &str, id: &str, start: u64, attributes: Value) -> crate::Span {
        let mut span = span(trace, id, start, attributes);
        span.start_time_ns += 1_600_000_000_000_000_000;
        span.end_time_ns += 1_600_000_000_000_000_000;
        span
    }

    fn llm(
        trace: &str,
        id: &str,
        start: u64,
        model: &str,
        prompt: &str,
        answer: &str,
    ) -> crate::Span {
        step_span(
            trace,
            id,
            start,
            json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": model,
                   "gen_ai.usage.input_tokens": 10, "gen_ai.usage.output_tokens": 5,
                   "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": prompt,
                   "gen_ai.completion.0.content": answer}),
        )
    }

    fn tool(trace: &str, id: &str, start: u64, name: &str, input: &str) -> crate::Span {
        step_span(
            trace,
            id,
            start,
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                   "gen_ai.tool.call.arguments": input, "gen_ai.tool.call.result": "ok"}),
        )
    }

    const NOW: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn identical_runs_have_no_divergence_and_each_kind_is_found_where_it_happens() {
        let fixture = TempStore::open("diff-kinds", crate::Config::default());
        let store = fixture.store();
        let base = |trace: &str| {
            vec![
                llm(trace, "1", 10, "gpt-4o", "plan", "search"),
                tool(trace, "2", 20, "search", "{\"q\":\"x\"}"),
                llm(trace, "3", 30, "gpt-4o", "answer", "done"),
            ]
        };
        let mut spans = base("same-a");
        spans.extend(base("same-b"));
        // Input: the tool got a different query.
        spans.extend([
            llm("input", "1", 10, "gpt-4o", "plan", "search"),
            tool("input", "2", 20, "search", "{\"q\":\"y\"}"),
            llm("input", "3", 30, "gpt-4o", "answer", "done"),
        ]);
        // Decision: same prompt, different answer at the last step.
        spans.extend([
            llm("decision", "1", 10, "gpt-4o", "plan", "search"),
            tool("decision", "2", 20, "search", "{\"q\":\"x\"}"),
            llm("decision", "3", 30, "gpt-4o", "answer", "gave up"),
        ]);
        // Config: the second model call swapped models.
        spans.extend([
            llm("config", "1", 10, "gpt-4o", "plan", "search"),
            tool("config", "2", 20, "search", "{\"q\":\"x\"}"),
            llm("config", "3", 30, "gpt-4o-mini", "answer", "done"),
        ]);
        // Structure: an extra tool call before the answer.
        spans.extend([
            llm("structure", "1", 10, "gpt-4o", "plan", "search"),
            tool("structure", "2", 20, "search", "{\"q\":\"x\"}"),
            tool("structure", "2b", 25, "fetch", "{\"url\":\"u\"}"),
            llm("structure", "3", 30, "gpt-4o", "answer", "done"),
        ]);
        store.ingest_batch(spans).expect("ingests");

        let diff = |b: &str| {
            store
                .diff_runs(Some(""), "trace:same-a", &format!("trace:{b}"), NOW)
                .expect("reads")
                .expect("both exist")
        };
        let same = diff("same-b");
        assert_eq!(same.first_divergence, None);
        assert!(same.aligned.iter().all(|entry| entry.op == Op::Same));
        assert_eq!(same.delta.steps, 0);
        assert_eq!(same.a.steps, 3);
        assert_eq!(same.a.tokens, 30);
        assert_eq!(same.a.outcome, attribution::Outcome::Success);

        for (run, kind, a, b) in [
            ("input", DivergenceKind::Input, 1, 1),
            ("decision", DivergenceKind::Decision, 2, 2),
            ("config", DivergenceKind::Config, 2, 2),
            ("structure", DivergenceKind::Structure, 2, 2),
        ] {
            let found = diff(run).first_divergence.expect(run);
            assert_eq!(
                (found.kind, found.a, found.b),
                (kind, a, b),
                "{run}: {found:?}"
            );
        }
        let config = diff("config");
        assert!(
            config
                .first_divergence
                .as_ref()
                .unwrap()
                .explanation
                .contains("gpt-4o-mini"),
            "{:?}",
            config.first_divergence
        );
        // A model swap is one changed entry, not a delete plus an insert.
        assert_eq!(config.aligned.len(), 3);
        let structure = diff("structure");
        assert_eq!(structure.aligned[2].op, Op::Insert);
        assert_eq!(structure.aligned[3].op, Op::Same, "the answer still aligns");
        assert_eq!(structure.delta.steps, 1);
    }

    #[test]
    fn a_parameter_change_with_the_same_model_is_config() {
        let fixture = TempStore::open("diff-params", crate::Config::default());
        let store = fixture.store();
        let mut cold = llm("cold", "1", 10, "gpt-4o", "hi", "hello");
        cold.attributes
            .insert("gen_ai.request.temperature".into(), json!(0.0));
        let mut hot = llm("hot", "1", 10, "gpt-4o", "hi", "hello");
        hot.attributes
            .insert("gen_ai.request.temperature".into(), json!(1.0));
        store.ingest_batch(vec![cold, hot]).expect("ingests");
        let found = store
            .diff_runs(Some(""), "trace:cold", "trace:hot", NOW)
            .expect("reads")
            .expect("both exist")
            .first_divergence
            .expect("differs");
        assert_eq!(found.kind, DivergenceKind::Config);
        assert!(
            found
                .explanation
                .contains("gen_ai.request.temperature 0.0 in a, 1.0 in b"),
            "{}",
            found.explanation
        );
    }

    #[test]
    fn a_side_past_the_span_cap_says_so_and_does_not_claim_an_outcome() {
        let fixture = TempStore::open("diff-span-cap", crate::Config::default());
        let store = fixture.store();
        let run = |trace: &str| {
            vec![
                llm(trace, "1", 10, "gpt-4o", "plan", "search"),
                tool(trace, "2", 20, "search", "{}"),
                llm(trace, "3", 30, "gpt-4o", "answer", "done"),
            ]
        };
        let mut spans = run("long");
        spans.extend(run("short").into_iter().take(2));
        store.ingest_batch(spans).expect("ingests");
        let diff = store
            .diff_runs_capped(Some(""), "trace:short", "trace:long", NOW, 2, 100)
            .expect("reads")
            .expect("both exist");
        assert!(diff.truncated);
        assert!(!diff.a.truncated, "two spans fit a cap of two");
        assert_eq!(diff.a.outcome, attribution::Outcome::Success);
        assert!(diff.b.truncated);
        assert_eq!(
            diff.b.outcome,
            attribution::Outcome::Unknown,
            "a run read from its beginning has not been read to its end"
        );
        assert_eq!(diff.b.steps, 2);
        assert_eq!(diff.b.tokens, 15, "summaries describe the spans read");
        // A step cap alone leaves the side's summary whole.
        let stepped = store
            .diff_runs_capped(Some(""), "trace:long", "trace:long", NOW, 100, 1)
            .expect("reads")
            .expect("both exist");
        assert!(stepped.truncated);
        assert!(!stepped.a.truncated);
        assert_eq!(stepped.a.outcome, attribution::Outcome::Success);
        assert_eq!(stepped.a.tokens, 30);
    }

    #[test]
    fn explanations_clip_the_producer_text_they_quote() {
        let fixture = TempStore::open("diff-clip", crate::Config::default());
        let store = fixture.store();
        let blob = json!({"stop": ["x".repeat(5_000)]});
        let mut left = llm("left", "1", 10, "m", "hi", "hello");
        left.attributes
            .insert("gen_ai.request.response_format".into(), blob.clone());
        let mut right = llm("right", "1", 10, "m", "hi", "hello");
        right
            .attributes
            .insert("gen_ai.request.response_format".into(), json!("text"));
        let long_name = "t".repeat(5_000);
        store
            .ingest_batch(vec![
                left,
                right,
                tool("insert-a", "1", 10, "search", "{}"),
                tool("insert-b", "1", 10, &long_name, "{}"),
            ])
            .expect("ingests");
        for (a, b) in [("left", "right"), ("insert-b", "insert-a")] {
            let found = store
                .diff_runs(Some(""), &format!("trace:{a}"), &format!("trace:{b}"), NOW)
                .expect("reads")
                .expect("both exist")
                .first_divergence
                .expect("differs");
            let length = found.explanation.chars().count();
            assert!(
                length <= MAX_EXPLANATION_CHARS + 1,
                "{length}: {}",
                found.explanation
            );
            assert!(found.explanation.contains('…'), "{}", found.explanation);
        }
    }

    #[test]
    fn a_missing_run_is_named() {
        let fixture = TempStore::open("diff-missing", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![llm("here", "1", 1, "m", "p", "r")])
            .expect("ingests");
        assert_eq!(
            store
                .diff_runs(Some(""), "trace:here", "trace:gone", NOW)
                .expect("reads"),
            Err(MissingRun("trace:gone".into()))
        );
        assert_eq!(
            store
                .diff_runs(Some(""), "trace:gone", "trace:here", NOW)
                .expect("reads"),
            Err(MissingRun("trace:gone".into()))
        );
    }
}
