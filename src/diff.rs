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
//! run under one compute budget, which every later phase spends too: the step
//! extraction; the alignment, which is quadratic in steps, so it compares
//! interned signatures rather than their (producer-sized) text, and checks the
//! clock every table row; the classification, which compares producer-sized
//! configurations pair by pair; and the explanation, which compares every
//! parameter of the divergent pair to count how many differ. Configurations
//! are compared under a meter that checks the clock as the comparison walks
//! them — not only between pairs, which would leave one pair's comparison as
//! long as its producer made it. A diff that finishes past the budget is
//! refused like one stopped at it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::attribution;
use crate::semconv::StepKind;
use crate::tape::{RunTape, TapeStep};
use crate::{Deadline, Result, Span, Store};

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

/// Signatures interned between two deadline checks.
const DEADLINE_CHECK_STEPS: usize = 64;

/// Units of comparison work between two deadline checks, where a unit is one
/// JSON node or one [`COMPARE_CHUNK_BYTES`] of text ([`Meter`]). A unit
/// costs nanoseconds to a microsecond, so a check every thousand of them is
/// invisible next to the work and overruns the budget by a fraction of a
/// millisecond at most.
const DEADLINE_CHECK_UNITS: usize = 1024;

/// Bytes of text compared as one unit. Text is compared a chunk at a time,
/// so a single producer-sized string cannot run on between two checks.
const COMPARE_CHUNK_BYTES: usize = 4096;

/// Differing parameters an explanation describes; the rest are counted.
const SHOWN_DIFFERENCES: usize = 3;

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
    /// Both runs are read, aligned, classified and explained under ONE
    /// compute budget; past it the diff is refused
    /// ([`crate::Error::DeadlineExceeded`]) wherever it had got to, finished
    /// or not. A run with no spans is [`MissingRun`], naming it.
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
        self.diff_tapes(a, &left, b, &right, now_ns, deadline)
            .map(Ok)
    }

    /// Two runs already read — `left` as run `a`, `right` as run `b` —
    /// aligned, classified and explained under the caller's `deadline`, so a
    /// request comparing one run with many (a run's forks) reads the shared
    /// side once and spends one budget across every comparison.
    pub(crate) fn diff_tapes(
        &self,
        a: &str,
        left: &RunTape,
        b: &str,
        right: &RunTape,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<RunDiff> {
        let side_a = self.side(a, left, now_ns);
        let side_b = self.side(b, right, now_ns);
        let aligned = align(&left.steps, &right.steps, deadline)?;
        let first_divergence = first_divergence(&aligned, &left.steps, &right.steps, deadline)?;
        let delta = Delta {
            cost_usd: side_b.cost_usd - side_a.cost_usd,
            tokens: signed(side_b.tokens) - signed(side_a.tokens),
            duration_ns: signed(side_b.duration_ns) - signed(side_a.duration_ns),
            errors: signed(side_b.errors as u64) - signed(side_a.errors as u64),
            steps: signed(side_b.steps as u64) - signed(side_a.steps as u64),
        };
        // The budget bounds the answer, not only the stretches between
        // checks: a diff that finished past it is refused like one stopped
        // there.
        Deadline::check(deadline, 0)?;
        Ok(RunDiff {
            a: side_a,
            b: side_b,
            aligned,
            first_divergence,
            delta,
            truncated: left.truncated || right.truncated,
        })
    }

    /// One run summarised as a side of a diff.
    pub(crate) fn side(&self, run: &str, tape: &RunTape, now_ns: u64) -> RunSide {
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

/// Aligns two tapes and classifies every entry, within `deadline`.
fn align(a: &[TapeStep], b: &[TapeStep], deadline: Option<Deadline>) -> Result<Vec<Aligned>> {
    let (left, right) = intern(a, b, deadline)?;
    let pairs = lcs_pairs(&left, &right, deadline)?;
    assemble(a, b, &pairs, deadline)
}

/// The alignment's entries from its matched pairs: each pair classified, and
/// the steps between pairs as config changes, deletions and insertions.
///
/// Spends the budget too, checked every [`DEADLINE_CHECK_STEPS`] entries:
/// classifying a pair compares two whole configurations and every entry
/// copies two fingerprints, all producer-sized, and a pair of runs whose
/// signatures all match reaches this with no quadratic part — no table row
/// at which the alignment checked the clock.
fn assemble(
    a: &[TapeStep],
    b: &[TapeStep],
    pairs: &[(usize, usize)],
    deadline: Option<Deadline>,
) -> Result<Vec<Aligned>> {
    let mut aligned = Vec::with_capacity(a.len().max(b.len()));
    let mut meter = Meter::new(deadline);
    let (mut next_a, mut next_b) = (0, 0);
    for &(pa, pb) in pairs {
        gap(a, b, next_a..pa, next_b..pb, &mut aligned, deadline)?;
        pace(&aligned, deadline)?;
        let why = classify(&a[pa], &b[pb], &mut meter)?;
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
    gap(
        a,
        b,
        next_a..a.len(),
        next_b..b.len(),
        &mut aligned,
        deadline,
    )?;
    Ok(aligned)
}

/// Checks the budget once every [`DEADLINE_CHECK_STEPS`] entries of `out`,
/// before the next one is built.
fn pace(out: &[Aligned], deadline: Option<Deadline>) -> Result<()> {
    if out.len() % DEADLINE_CHECK_STEPS == 0 {
        Deadline::check(deadline, 0)?;
    }
    Ok(())
}

/// Both tapes' signatures as small integers, equal exactly when the
/// signatures are equal.
///
/// Signatures carry producer-chosen names of any length, and the alignment
/// compares them pairwise — up to a hundred million comparisons for two
/// 5,000-step runs. Comparing strings there would cost the length of their
/// common prefix every time; interning costs one hash of each signature, and
/// leaves every comparison in the quadratic part constant-time.
fn intern<'t>(
    a: &'t [TapeStep],
    b: &'t [TapeStep],
    deadline: Option<Deadline>,
) -> Result<(Vec<u32>, Vec<u32>)> {
    let mut ids: HashMap<&'t str, u32> = HashMap::new();
    let mut id_of = |step: &'t TapeStep| {
        let next = ids.len() as u32;
        *ids.entry(step.detail.signature.as_str()).or_insert(next)
    };
    let mut left = Vec::with_capacity(a.len());
    let mut right = Vec::with_capacity(b.len());
    for (count, step) in a.iter().enumerate() {
        if count % DEADLINE_CHECK_STEPS == 0 {
            Deadline::check(deadline, 0)?;
        }
        left.push(id_of(step));
    }
    for (count, step) in b.iter().enumerate() {
        if count % DEADLINE_CHECK_STEPS == 0 {
            Deadline::check(deadline, 0)?;
        }
        right.push(id_of(step));
    }
    Ok((left, right))
}

/// Why two steps with the same signature differ, or `None` when they do not.
/// The configurations are compared under `meter`: they are as large as their
/// producer made them, and the pair was checked against the budget only
/// before this began.
fn classify(a: &TapeStep, b: &TapeStep, meter: &mut Meter) -> Result<Option<DivergenceKind>> {
    if a.kind == StepKind::Llm && !maps_equal(&a.detail.config, &b.detail.config, meter)? {
        return Ok(Some(DivergenceKind::Config));
    }
    if a.request_digest != b.request_digest {
        return Ok(Some(DivergenceKind::Input));
    }
    if a.response_digest != b.response_digest {
        return Ok(Some(DivergenceKind::Decision));
    }
    Ok(None)
}

/// Comparison work spent against a deadline, checked once every
/// [`DEADLINE_CHECK_UNITS`] units however the units fall.
///
/// A check per aligned pair, or per configuration key, still leaves one
/// comparison between two checks — and a comparison of two producer-sized
/// values is as long as the producer made them. Counting the walk itself
/// bounds every stretch between checks by the meter, not by the data.
struct Meter {
    deadline: Option<Deadline>,
    since_check: usize,
}

impl Meter {
    fn new(deadline: Option<Deadline>) -> Self {
        Self {
            deadline,
            since_check: 0,
        }
    }

    /// Counts one unit of work, checking the deadline once enough are spent.
    fn spend(&mut self) -> Result<()> {
        self.since_check += 1;
        if self.since_check >= DEADLINE_CHECK_UNITS {
            self.since_check = 0;
            Deadline::check(self.deadline, 0)?;
        }
        Ok(())
    }
}

/// `a == b`, answered exactly as `Value`'s own equality answers it, but
/// walked under `meter`.
fn equal(a: &Value, b: &Value, meter: &mut Meter) -> Result<bool> {
    meter.spend()?;
    match (a, b) {
        (Value::String(a), Value::String(b)) => text_equal(a, b, meter),
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (a, b) in a.iter().zip(b) {
                if !equal(a, b, meter)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Object(a), Value::Object(b)) => maps_equal(a, b, meter),
        // Nulls, booleans and numbers compare in constant time, and values
        // of two different kinds are unequal on sight.
        _ => Ok(a == b),
    }
}

/// `a == b` for two configurations or JSON objects, under `meter`.
///
/// Pairwise in key order, which is how `Map`'s own equality compares: the
/// map is a `BTreeMap`, because nothing enables `serde_json`'s
/// `preserve_order` (`configurations_iterate_in_key_order` fails if anything
/// does).
fn maps_equal(a: &Map<String, Value>, b: &Map<String, Value>, meter: &mut Meter) -> Result<bool> {
    if a.len() != b.len() {
        return Ok(false);
    }
    for ((key_a, value_a), (key_b, value_b)) in a.iter().zip(b) {
        if !text_equal(key_a, key_b, meter)? || !equal(value_a, value_b, meter)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `a == b` for two strings, under `meter`.
fn text_equal(a: &str, b: &str, meter: &mut Meter) -> Result<bool> {
    Ok(a.len() == b.len() && text_order(a, b, meter)? == Ordering::Equal)
}

/// `a.cmp(b)` for two strings — byte order, as `str` orders — compared
/// [`COMPARE_CHUNK_BYTES`] at a time under `meter`.
fn text_order(a: &str, b: &str, meter: &mut Meter) -> Result<Ordering> {
    let pieces = a
        .as_bytes()
        .chunks(COMPARE_CHUNK_BYTES)
        .zip(b.as_bytes().chunks(COMPARE_CHUNK_BYTES));
    for (a, b) in pieces {
        meter.spend()?;
        // Unequal pieces decide it, a shorter last piece included: it is a
        // prefix of its partner exactly when its string is a prefix of the
        // other.
        match a.cmp(b) {
            Ordering::Equal => {}
            order => return Ok(order),
        }
    }
    Ok(a.len().cmp(&b.len()))
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
    deadline: Option<Deadline>,
) -> Result<()> {
    let (mut i, mut j) = (left.start, right.start);
    while i < left.end || j < right.end {
        pace(out, deadline)?;
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
    Ok(())
}

/// The first entry that is not `same`, explained — within `deadline`, which
/// the scan checks every [`DEADLINE_CHECK_STEPS`] entries, and the
/// explanation once more before it begins and then throughout its comparison
/// of the two steps' configurations.
fn first_divergence(
    aligned: &[Aligned],
    a: &[TapeStep],
    b: &[TapeStep],
    deadline: Option<Deadline>,
) -> Result<Option<FirstDivergence>> {
    // The next index on each side, for an entry the side does not have.
    let (mut next_a, mut next_b) = (0, 0);
    for (count, entry) in aligned.iter().enumerate() {
        if count % DEADLINE_CHECK_STEPS == 0 {
            Deadline::check(deadline, 0)?;
        }
        if entry.op != Op::Same {
            Deadline::check(deadline, 0)?;
            let kind = entry.why.unwrap_or(DivergenceKind::Structure);
            let explanation = explain(entry, kind, a, b, &mut Meter::new(deadline))?;
            let explanation = clip(&explanation, MAX_EXPLANATION_CHARS);
            return Ok(Some(FirstDivergence {
                a: entry.a.unwrap_or(next_a),
                b: entry.b.unwrap_or(next_b),
                kind,
                explanation,
            }));
        }
        next_a = entry.a.map_or(next_a, |index| index + 1);
        next_b = entry.b.map_or(next_b, |index| index + 1);
    }
    Ok(None)
}

fn preview(digest: Option<&String>) -> String {
    digest.map_or_else(
        || "none".to_owned(),
        |digest| digest[..digest.len().min(DIGEST_PREVIEW)].to_owned(),
    )
}

/// `text`, cut to `max` characters with an ellipsis when longer. Reads at
/// most `max + 1` characters of it, however long it is.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

fn quote(text: &str) -> String {
    clip(text, QUOTED_CHARS)
}

/// `value` as compact JSON, quoted as [`quote`] quotes text — but written
/// only as far as the quote shows. A configuration value is producer-sized,
/// and writing a megabyte of it out to keep eighty characters is work the
/// budget would not see.
///
/// Written here rather than by `serde_json`, which scans a whole string for
/// characters to escape before it writes any of it: a writer refusing
/// everything past the quote bounded the memory that took, not the time.
fn quote_json(value: &Value) -> String {
    let mut prefix = JsonPrefix::default();
    // Its only error is the one that says the quote is full.
    let _ = prefix.value(value);
    quote(&prefix.text)
}

/// Compact JSON exactly as `serde_json` writes it, cut off as soon as it
/// holds more than [`QUOTED_CHARS`] characters — one past what a quote
/// shows, so a value that does not fit is always seen not to.
///
/// Every value writes a character before anything inside it is visited, and
/// a string is written a character at a time, so the walk ends within a
/// quote's length of characters and values whatever the value's size: a
/// sixty-megabyte string costs what an eighty-character one does, with no
/// deadline needed to stop it.
#[derive(Default)]
struct JsonPrefix {
    text: String,
    chars: usize,
}

/// A [`JsonPrefix`] holds everything its quote shows.
struct QuoteFull;

impl JsonPrefix {
    fn push(&mut self, piece: &str) -> std::result::Result<(), QuoteFull> {
        self.text.push_str(piece);
        self.chars += piece.chars().count();
        if self.chars > QUOTED_CHARS {
            return Err(QuoteFull);
        }
        Ok(())
    }

    fn value(&mut self, value: &Value) -> std::result::Result<(), QuoteFull> {
        match value {
            Value::Null => self.push("null"),
            Value::Bool(true) => self.push("true"),
            Value::Bool(false) => self.push("false"),
            // Without `arbitrary_precision` a number is an integer or a
            // finite double, a few dozen characters at most, and its
            // `Display` is the text serde_json writes for it.
            Value::Number(number) => self.push(&number.to_string()),
            Value::String(text) => self.string(text),
            Value::Array(items) => {
                self.push("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        self.push(",")?;
                    }
                    self.value(item)?;
                }
                self.push("]")
            }
            Value::Object(members) => {
                self.push("{")?;
                for (index, (key, item)) in members.iter().enumerate() {
                    if index > 0 {
                        self.push(",")?;
                    }
                    self.string(key)?;
                    self.push(":")?;
                    self.value(item)?;
                }
                self.push("}")
            }
        }
    }

    /// `text` as a JSON string, escaped as serde_json escapes it: the quote
    /// and the backslash, the five controls with short escapes, every other
    /// control as `\u00xx` in lowercase hex, and everything else as itself.
    fn string(&mut self, text: &str) -> std::result::Result<(), QuoteFull> {
        self.push("\"")?;
        let mut buffer = [0u8; 4];
        for c in text.chars() {
            let escaped;
            let piece = match c {
                '"' => "\\\"",
                '\\' => "\\\\",
                '\u{8}' => "\\b",
                '\u{c}' => "\\f",
                '\n' => "\\n",
                '\r' => "\\r",
                '\t' => "\\t",
                c if c < ' ' => {
                    escaped = format!("\\u{:04x}", u32::from(c));
                    &escaped
                }
                c => c.encode_utf8(&mut buffer),
            };
            self.push(piece)?;
        }
        self.push("\"")
    }
}

/// The sentence for one divergent entry. Only a parameter change compares
/// producer-sized data, and does it under `meter`.
fn explain(
    entry: &Aligned,
    kind: DivergenceKind,
    a: &[TapeStep],
    b: &[TapeStep],
    meter: &mut Meter,
) -> Result<String> {
    Ok(match (entry.a.map(|i| &a[i]), entry.b.map(|j| &b[j])) {
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
                    let keys =
                        config_differences(&left.detail.config, &right.detail.config, meter)?;
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
    })
}

/// The configuration keys that differ, in key order, with both values: the
/// first [`SHOWN_DIFFERENCES`] described and the rest counted.
///
/// The two configurations are walked side by side in key order (see
/// [`maps_equal`]), every key compared under `meter`. Counting the rest
/// still compares every one of them, but builds nothing for it — a
/// description is quoted text, and thousands of them would be work and
/// memory spent on sentences the explanation clips away.
fn config_differences(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    meter: &mut Meter,
) -> Result<String> {
    let mut shown: Vec<String> = Vec::with_capacity(SHOWN_DIFFERENCES);
    let mut more = 0usize;
    let mut differs = |key: &str, left: Option<&Value>, right: Option<&Value>| {
        if shown.len() == SHOWN_DIFFERENCES {
            more += 1;
            return;
        }
        let show = |value: Option<&Value>| value.map_or_else(|| "unset".to_owned(), quote_json);
        shown.push(format!(
            "{} {} in a, {} in b",
            quote(key),
            show(left),
            show(right)
        ));
    };
    let (mut left, mut right) = (a.iter().peekable(), b.iter().peekable());
    loop {
        // A key only one side has is a step of the walk like any other.
        meter.spend()?;
        let order = match (left.peek(), right.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((key_a, _)), Some((key_b, _))) => text_order(key_a, key_b, meter)?,
        };
        match order {
            Ordering::Less => {
                if let Some((key, value)) = left.next() {
                    differs(key, Some(value), None);
                }
            }
            Ordering::Greater => {
                if let Some((key, value)) = right.next() {
                    differs(key, None, Some(value));
                }
            }
            Ordering::Equal => {
                if let (Some((key, value_a)), Some((_, value_b))) = (left.next(), right.next()) {
                    if !equal(value_a, value_b, meter)? {
                        differs(key, Some(value_a), Some(value_b));
                    }
                }
            }
        }
    }
    let mut text = shown.join("; ");
    if more > 0 {
        text.push_str(&format!("; and {more} more"));
    }
    Ok(text)
}

// ---------------------------------------------------------------- alignment

/// The index pairs of a longest common subsequence of `a` and `b`, in order,
/// or a deadline refusal: the quadratic part checks the clock once per table
/// row, so it overruns its budget by one row at most.
///
/// The common prefix and suffix are matched directly — two runs of the same
/// agent usually share most of both, which leaves only the middle for the
/// quadratic part — and the middle is aligned in linear space.
fn lcs_pairs(a: &[u32], b: &[u32], deadline: Option<Deadline>) -> Result<Vec<(usize, usize)>> {
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
        (prefix, prefix),
        deadline,
        &mut pairs,
    )?;
    let (a_tail, b_tail) = (a.len() - suffix, b.len() - suffix);
    pairs.extend((0..suffix).map(|offset| (a_tail + offset, b_tail + offset)));
    Ok(pairs)
}

/// Hirschberg's divide and conquer: split `a` in half, find where the two
/// halves' LCS lengths against `b` sum to the maximum, and recurse on each
/// side of that split. Linear space; ties take the earliest split, so the
/// alignment is deterministic. `offsets` are `a`'s and `b`'s positions in
/// the whole tapes.
fn hirschberg(
    a: &[u32],
    b: &[u32],
    offsets: (usize, usize),
    deadline: Option<Deadline>,
    out: &mut Vec<(usize, usize)>,
) -> Result<()> {
    if a.is_empty() || b.is_empty() {
        return Ok(());
    }
    if a.len().saturating_mul(b.len()) <= DIRECT_TABLE_CELLS || a.len() == 1 {
        return direct(a, b, offsets, deadline, out);
    }
    let middle = a.len() / 2;
    let forward = lcs_row(a[..middle].iter(), b.iter(), b.len(), deadline)?;
    let backward = lcs_row(a[middle..].iter().rev(), b.iter().rev(), b.len(), deadline)?;
    let split = (0..=b.len())
        .max_by(|&left, &right| {
            let score = |j: usize| forward[j] + backward[b.len() - j];
            // `max_by` keeps the LAST maximum; comparing in reverse on ties
            // makes it keep the first.
            score(left).cmp(&score(right)).then(right.cmp(&left))
        })
        .unwrap_or(0);
    let (a_offset, b_offset) = offsets;
    hirschberg(&a[..middle], &b[..split], offsets, deadline, out)?;
    hirschberg(
        &a[middle..],
        &b[split..],
        (a_offset + middle, b_offset + split),
        deadline,
        out,
    )
}

/// The last row of the LCS-length table: entry `j` is the LCS length of all
/// of `a` against the first `j` items of `b`.
fn lcs_row<'s>(
    a: impl Iterator<Item = &'s u32>,
    b: impl Iterator<Item = &'s u32> + Clone,
    b_len: usize,
    deadline: Option<Deadline>,
) -> Result<Vec<u32>> {
    let mut previous = vec![0u32; b_len + 1];
    let mut current = vec![0u32; b_len + 1];
    for left in a {
        Deadline::check(deadline, 0)?;
        for (j, right) in b.clone().enumerate() {
            current[j + 1] = if left == right {
                previous[j] + 1
            } else {
                current[j].max(previous[j + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(previous)
}

/// The full table and its backtrack, for small subproblems.
fn direct(
    a: &[u32],
    b: &[u32],
    (a_offset, b_offset): (usize, usize),
    deadline: Option<Deadline>,
    out: &mut Vec<(usize, usize)>,
) -> Result<()> {
    let width = b.len() + 1;
    let mut table = vec![0u32; (a.len() + 1) * width];
    for i in 1..=a.len() {
        Deadline::check(deadline, 0)?;
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
    Ok(())
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

    /// The signatures as the interner would number them.
    fn ids<'s>(a: &[&'s str], b: &[&'s str]) -> (Vec<u32>, Vec<u32>) {
        let mut ids: HashMap<&'s str, u32> = HashMap::new();
        let mut id = |text: &&'s str| {
            let next = ids.len() as u32;
            *ids.entry(*text).or_insert(next)
        };
        let left = a.iter().map(&mut id).collect();
        let right = b.iter().map(&mut id).collect();
        (left, right)
    }

    /// [`lcs_pairs`] over signatures, with no deadline.
    fn string_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
        let (left, right) = ids(a, b);
        lcs_pairs(&left, &right, None).expect("no deadline to miss")
    }

    #[test]
    fn the_alignment_stops_at_the_deadline_on_both_paths() {
        let spent = Deadline::starting(Instant::now(), Some(std::time::Duration::ZERO));
        // Nothing shared at either end, so the quadratic part runs: a small
        // pair takes the direct table, a large one Hirschberg's rows.
        for size in [10, 2000] {
            let a: Vec<u32> = (0..size).collect();
            let b: Vec<u32> = (size..2 * size).collect();
            assert!(
                matches!(
                    lcs_pairs(&a, &b, spent),
                    Err(crate::Error::DeadlineExceeded(_))
                ),
                "{size}"
            );
        }
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
            let pairs = string_pairs(&a, &b);
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
        let pairs = string_pairs(&a, &b);
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
    fn alignment_spends_the_query_deadline_however_long_the_names() {
        // Names of one length sharing all but their last bytes: every string
        // comparison scans the whole name, and an alignment of two runs with
        // nothing in common compares every pair — tens of millions of them,
        // seconds past any budget when each scans 4 KiB. Whether a debug
        // build finishes inside the budget or is refused at it, it must not
        // run on far past it.
        let budget = std::time::Duration::from_secs(2);
        let config = crate::Config {
            query_deadline: Some(budget),
            durability: crate::Durability::Buffered,
            ..crate::Config::default()
        };
        let fixture = TempStore::open("diff-deadline", config);
        let store = fixture.store();
        let prefix = "p".repeat(4096);
        let steps = MAX_DIFF_STEPS as u64;
        for side in ["a", "b"] {
            let spans: Vec<crate::Span> = (0..steps)
                .map(|index| {
                    tool(
                        &format!("long-{side}"),
                        &format!("{index:05}"),
                        index,
                        &format!("{prefix}{side}{index:05}"),
                        "{}",
                    )
                })
                .collect();
            store.ingest_batch(spans).expect("ingests");
        }
        let started = Instant::now();
        let outcome = store.diff_runs(Some(""), "trace:long-a", "trace:long-b", NOW);
        let elapsed = started.elapsed();
        match outcome {
            Ok(Ok(diff)) => assert_eq!(diff.aligned.len(), 2 * MAX_DIFF_STEPS),
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a diff or a deadline refusal, got {other:?}"),
        }
        assert!(
            elapsed < budget + std::time::Duration::from_secs(3),
            "the diff ran {elapsed:?} against a {budget:?} budget"
        );
    }

    #[test]
    fn classification_and_explanation_spend_the_deadline_too() {
        // Two runs whose signatures all match: the alignment is their common
        // prefix, with no quadratic part and so no table row to check the
        // clock at. What follows it — classifying every pair, which compares
        // whole producer-sized configurations, and explaining the first
        // divergence — is work the budget must still bound.
        let fixture = TempStore::open("diff-late-phases", crate::Config::default());
        let store = fixture.store();
        let blob = json!({"stop": ["s".repeat(4096)]});
        for trace in ["late-a", "late-b"] {
            let spans: Vec<crate::Span> = (0..200u64)
                .map(|index| {
                    let mut step = llm(trace, &format!("{index:03}"), index, "gpt-4o", "go", "ok");
                    step.attributes
                        .insert("gen_ai.request.response_format".into(), blob.clone());
                    // The runs part at their last step, so there is a
                    // divergence to explain.
                    if trace == "late-b" && index == 199 {
                        step.attributes
                            .insert("gen_ai.request.temperature".into(), json!(1.0));
                    }
                    step
                })
                .collect();
            store.ingest_batch(spans).expect("ingests");
        }
        let tape = |run: &str| {
            store
                .run_tape(Some(""), run, MAX_DIFF_SPANS, MAX_DIFF_STEPS, None)
                .expect("reads")
                .steps
        };
        let (a, b) = (tape("trace:late-a"), tape("trace:late-b"));
        let (left, right) = intern(&a, &b, None).expect("no deadline to miss");
        let pairs = lcs_pairs(&left, &right, None).expect("no deadline to miss");
        assert_eq!(pairs.len(), a.len(), "every step aligns");

        let spent = Deadline::starting(Instant::now(), Some(std::time::Duration::ZERO));
        assert!(
            matches!(
                assemble(&a, &b, &pairs, spent),
                Err(crate::Error::DeadlineExceeded(_))
            ),
            "classifying the pairs spends the budget"
        );
        let aligned = assemble(&a, &b, &pairs, None).expect("no deadline to miss");
        assert!(
            matches!(
                first_divergence(&aligned, &a, &b, spent),
                Err(crate::Error::DeadlineExceeded(_))
            ),
            "explaining the divergence spends the budget"
        );
        // With budget left, the same work gives the same answer as ever.
        let found = first_divergence(&aligned, &a, &b, None)
            .expect("no deadline to miss")
            .expect("the runs part");
        assert_eq!(
            (found.kind, found.a, found.b),
            (DivergenceKind::Config, 199, 199)
        );
    }

    /// One llm step from each of two runs, read as the differ reads them, for
    /// a test to give configurations of its own — built in memory, because
    /// ingesting producer-sized configurations would time the store, not the
    /// differ.
    fn one_step_each(store: &Store) -> (Vec<TapeStep>, Vec<TapeStep>) {
        store
            .ingest_batch(vec![
                llm("one-a", "1", 10, "gpt-4o", "go", "ok"),
                llm("one-b", "1", 10, "gpt-4o", "go", "ok"),
            ])
            .expect("ingests");
        let tape = |run: &str| {
            store
                .run_tape(Some(""), run, MAX_DIFF_SPANS, MAX_DIFF_STEPS, None)
                .expect("reads")
                .steps
        };
        (tape("trace:one-a"), tape("trace:one-b"))
    }

    /// `length` numbers, and the same numbers with only the last one changed:
    /// a pair that compares equal right up to its end.
    fn nearly_equal(length: usize) -> (Value, Value) {
        let same: Vec<Value> = (0..length).map(Value::from).collect();
        let mut changed = same.clone();
        changed[length - 1] = json!(-1);
        (Value::Array(same), Value::Array(changed))
    }

    /// What `work` costs with no budget on THIS machine, in THIS build
    /// profile — the fastest of three runs, so a stall in one cannot inflate
    /// the yardstick a budgeted run of the same work is held against.
    fn unbudgeted_cost<T>(mut work: impl FnMut() -> T) -> std::time::Duration {
        (0..3)
            .map(|_| {
                let started = Instant::now();
                std::hint::black_box(work());
                started.elapsed()
            })
            .min()
            .expect("three runs")
    }

    /// Runs `work` under an eighth of `unbudgeted`, three times, and requires
    /// every run to be refused at the deadline — and the fastest well before
    /// the whole work would have finished, which is what tells a check
    /// INSIDE the work from one after it. The fastest of three, like the
    /// yardstick, so one stall on a busy machine is not read as an escape.
    fn assert_stops_inside<T: std::fmt::Debug>(
        what: &str,
        unbudgeted: std::time::Duration,
        work: impl Fn(Option<Deadline>) -> Result<T>,
    ) {
        let budget = unbudgeted / 8;
        let fastest = (0..3)
            .map(|_| {
                let started = Instant::now();
                let outcome = work(Deadline::starting(started, Some(budget)));
                let elapsed = started.elapsed();
                match outcome {
                    Err(crate::Error::DeadlineExceeded(_)) => elapsed,
                    other => panic!(
                        "{what} ran past its {budget:?} budget: {other:?} after {elapsed:?} \
                         (unbudgeted {unbudgeted:?})"
                    ),
                }
            })
            .min()
            .expect("three runs");
        assert!(
            fastest < unbudgeted / 2,
            "{what} was refused only after {fastest:?} of a {budget:?} budget \
             (unbudgeted {unbudgeted:?})"
        );
    }

    #[test]
    fn explaining_a_config_change_stops_at_the_deadline_however_many_parameters() {
        // Many parameters whose values agree right up to their last element.
        // Telling the configurations apart is instant — b has one more key —
        // so classification is cheap and the pair is checked against the
        // budget before it is explained; describing HOW they differ compares
        // every value whole. That walk is producer-sized and has to check
        // the clock as it goes, not leave it to whoever looks next.
        const PARAMETERS: usize = 400;
        const LENGTH: usize = 2_000;
        let fixture = TempStore::open("diff-explain-budget", crate::Config::default());
        let (mut a, mut b) = one_step_each(fixture.store());
        for index in 0..PARAMETERS {
            let key = format!("gen_ai.request.p{index:04}");
            let (same, changed) = nearly_equal(LENGTH);
            a[0].detail.config.insert(key.clone(), same);
            b[0].detail.config.insert(key, changed);
        }
        b[0].detail
            .config
            .insert("gen_ai.request.zz".into(), json!(1));
        let aligned = assemble(&a, &b, &[(0, 0)], None).expect("no deadline to miss");
        assert_eq!(aligned[0].why, Some(DivergenceKind::Config));

        let unbudgeted = unbudgeted_cost(|| {
            first_divergence(&aligned, &a, &b, None)
                .expect("no deadline to miss")
                .expect("the runs part")
        });
        assert_stops_inside("the explanation", unbudgeted, |deadline| {
            first_divergence(&aligned, &a, &b, deadline)
        });
    }

    #[test]
    fn classifying_a_pair_stops_at_the_deadline_however_large_its_configuration() {
        // One parameter whose two values agree right up to their last
        // element: the pair is checked against the budget before it is
        // classified, and comparing its configurations is then one walk
        // over the whole value — which has to check the clock as it goes.
        const LENGTH: usize = 800_000;
        let fixture = TempStore::open("diff-classify-budget", crate::Config::default());
        let (mut a, mut b) = one_step_each(fixture.store());
        let (same, changed) = nearly_equal(LENGTH);
        let key = "gen_ai.request.response_format".to_owned();
        a[0].detail.config.insert(key.clone(), same);
        b[0].detail.config.insert(key, changed);

        let unbudgeted = unbudgeted_cost(|| {
            let aligned = assemble(&a, &b, &[(0, 0)], None).expect("no deadline to miss");
            assert_eq!(aligned[0].why, Some(DivergenceKind::Config));
        });
        assert_stops_inside("classifying the pair", unbudgeted, |deadline| {
            assemble(&a, &b, &[(0, 0)], deadline)
        });
    }

    #[test]
    fn metered_equality_answers_as_value_equality_does() {
        let long = "x".repeat(3 * COMPARE_CHUNK_BYTES + 7);
        let mut long_changed = long.clone();
        long_changed.push('y');
        long_changed.remove(0);
        let values = [
            json!(null),
            json!(false),
            json!(true),
            json!(0),
            json!(0.0),
            json!(-1),
            json!(u64::MAX),
            json!(""),
            json!("a"),
            json!(long),
            json!(long_changed),
            json!([]),
            json!([1, 2]),
            json!([1, 2, 3]),
            json!([2, 1]),
            json!({}),
            json!({"a": 1}),
            json!({"a": 2}),
            json!({"b": 1}),
            json!({"a": 1, "b": [1, {"c": null}]}),
            json!({"a": 1, "b": [1, {"c": false}]}),
        ];
        for left in &values {
            for right in &values {
                let metered = equal(left, right, &mut Meter::new(None)).expect("no deadline");
                assert_eq!(metered, left == right, "{left} vs {right}");
            }
        }
        // Keys are walked in order by the same chunked comparison.
        let mut prefix = long.clone();
        prefix.truncate(2 * COMPARE_CHUNK_BYTES);
        let texts = ["", "a", "ab", "b", &long, &long_changed, &prefix];
        for left in texts {
            for right in texts {
                let metered = text_order(left, right, &mut Meter::new(None)).expect("no deadline");
                assert_eq!(
                    metered,
                    left.cmp(right),
                    "{} vs {}",
                    left.len(),
                    right.len()
                );
            }
        }
    }

    #[test]
    fn a_config_explanation_describes_three_differences_and_counts_the_rest() {
        let config = |pairs: &[(&str, Value)]| -> Map<String, Value> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect()
        };
        let a = config(&[
            ("a", json!(1)),
            ("b", json!(1)),
            ("c", json!(1)),
            ("same", json!([1, 2])),
            ("x", json!(1)),
            ("y", json!(1)),
        ]);
        let b = config(&[
            ("b", json!(2)),
            ("c", json!(2)),
            ("d", json!(2)),
            ("same", json!([1, 2])),
            ("x", json!(2)),
            ("z", json!(2)),
        ]);
        // Seven keys differ — one only in a, one only in b, the rest by
        // value — and `same` does not. The first three in key order are
        // described, each side's value or `unset`; the other four counted.
        let text = config_differences(&a, &b, &mut Meter::new(None)).expect("no deadline");
        assert_eq!(
            text,
            "a 1 in a, unset in b; b 1 in a, 2 in b; c 1 in a, 2 in b; and 4 more"
        );
        let few = config_differences(&a, &config(&[("a", json!(1))]), &mut Meter::new(None))
            .expect("no deadline");
        assert!(few.starts_with("b 1 in a, unset in b; "), "{few}");
        assert!(few.ends_with("; and 2 more"), "{few}");
        assert_eq!(
            config_differences(&a, &a, &mut Meter::new(None)).expect("no deadline"),
            ""
        );
    }

    #[test]
    fn configurations_iterate_in_key_order() {
        // `maps_equal` compares configurations pair by pair and
        // `config_differences` walks two side by side, both in key order —
        // serde_json's default `BTreeMap`. Its `preserve_order` feature
        // would make `Map` an insertion-ordered `IndexMap`, and both would
        // answer wrongly without anything else failing.
        let mut map = Map::new();
        map.insert("b".to_owned(), json!(1));
        map.insert("a".to_owned(), json!(2));
        assert_eq!(map.keys().collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn config_values_are_quoted_without_serializing_them_whole() {
        let huge = json!({"stop": ["é".repeat(1 << 20)]});
        let quoted = quote_json(&huge);
        assert_eq!(quoted.chars().count(), QUOTED_CHARS + 1, "{quoted}");
        assert!(quoted.ends_with('…'), "{quoted}");
        assert!(quoted.starts_with("{\"stop\":[\"éé"), "{quoted}");
        // A value that fits is quoted whole, exactly as serialized.
        assert_eq!(quote_json(&json!(0.5)), "0.5");
        assert_eq!(quote_json(&json!({"a": [1, 2]})), "{\"a\":[1,2]}");
    }

    #[test]
    fn a_quoted_value_reads_as_serde_json_writes_it() {
        // The quote is written by hand so it can stop early; what it shows
        // must still be serde_json's compact form, character for character,
        // whatever the value — escapes, controls, wide characters, numbers
        // of every kind, and lengths either side of the quote's edge.
        let mut values = vec![
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-17),
            json!(u64::MAX),
            json!(i64::MIN),
            json!(0.5),
            json!(-1.25e-7),
            json!(1e300),
            json!(""),
            json!([]),
            json!({}),
            json!([[], {}, [[]]]),
            json!("quote \" backslash \\ slash / bell \u{7} nul \u{0} del \u{7f}"),
            json!("\u{8}\u{c}\n\r\t\u{1f} é 中 😀"),
            json!({"k\"ey\n": ["v\\", {"": null}], "z": 1.5}),
            json!((0..100).collect::<Vec<_>>()),
            json!({"stop": ["\n".repeat(200)]}),
        ];
        for length in [
            QUOTED_CHARS - 3,
            QUOTED_CHARS - 2,
            QUOTED_CHARS - 1,
            QUOTED_CHARS,
        ] {
            values.push(json!("x".repeat(length)));
            values.push(json!("é".repeat(length)));
            values.push(json!("\t".repeat(length / 2)));
            values.push(json!(["é".repeat(length - 4)]));
        }
        for value in &values {
            let whole = serde_json::to_string(value).expect("serializes");
            assert_eq!(quote_json(value), quote(&whole), "{whole}");
        }
    }

    #[test]
    fn explaining_a_config_change_costs_the_same_however_long_its_strings() {
        // A parameter whose value is one long string with nothing in it to
        // escape. serde_json scans a whole string for escapes before writing
        // any of it, so a writer that refused bytes past the quote's length
        // bounded the memory and not the time: the explanation — which
        // checks the clock between comparisons, not inside a write — ran on
        // past the deadline for as long as the string was long.
        const LENGTH: usize = 16 << 20;
        let fixture = TempStore::open("diff-explain-long-string", crate::Config::default());
        let (mut a, mut b) = one_step_each(fixture.store());
        let long = json!("x".repeat(LENGTH));
        let key = "gen_ai.request.stop".to_owned();
        a[0].detail.config.insert(key.clone(), long.clone());
        // A different length, so telling the values apart is instant and
        // describing them is all the work there is.
        b[0].detail.config.insert(key, json!("y"));
        let aligned = assemble(&a, &b, &[(0, 0)], None).expect("no deadline to miss");
        assert_eq!(aligned[0].why, Some(DivergenceKind::Config));
        let found = first_divergence(&aligned, &a, &b, None)
            .expect("no deadline to miss")
            .expect("the runs part");
        let shown = format!("gen_ai.request.stop \"{}…", "x".repeat(QUOTED_CHARS - 1));
        assert!(found.explanation.contains(&shown), "{}", found.explanation);
        assert!(
            found.explanation.ends_with(" in a, \"y\" in b"),
            "{}",
            found.explanation
        );

        // What writing the value out whole costs on THIS machine, in THIS
        // build profile — the scan an unbounded quote waits for.
        let scan = unbudgeted_cost(|| serde_json::to_writer(std::io::sink(), &long));
        let budget = scan / 8;
        let fastest = (0..3)
            .map(|_| {
                let started = Instant::now();
                let outcome =
                    first_divergence(&aligned, &a, &b, Deadline::starting(started, Some(budget)));
                let elapsed = started.elapsed();
                match outcome {
                    Ok(Some(_)) | Err(crate::Error::DeadlineExceeded(_)) => elapsed,
                    other => panic!("expected an explanation or a refusal, got {other:?}"),
                }
            })
            .min()
            .expect("three runs");
        assert!(
            fastest < scan / 2,
            "explaining took {fastest:?} against a {budget:?} budget; \
             writing the value whole takes {scan:?}"
        );
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
