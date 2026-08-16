//! Attribution: what a run did, and which step broke it — computed here
//! rather than eyeballed by whoever reads the trace.
//!
//! The category this belongs to has solved capture. What it has not solved is
//! the question after capture: a platform replays a trace and leaves a human
//! to work out which step broke the run. Traza's MCP prompts did the same
//! thing one level up — they told the model *"repeated sibling names are a
//! retry storm, deep chains are a loop"* and left it to squint at a rendered
//! tree. That instruction is a branch, and this module's own house rule says
//! a prompt that wants a branch is a tool. So the branch moved here, where it
//! can be given evidence and held to it.
//!
//! # What this module refuses to do
//!
//! Every rule below fires on telemetry that already exists. That constraint
//! is the whole design, and it was arrived at by getting it wrong first: an
//! earlier draft rested on `session.outcome` and a `relation: "retry-of"`
//! link, and both are conventions Traza invented. Nothing in the OpenLLMetry
//! or OTel GenAI ecosystem emits either. A detector built on them would have
//! been a closed loop — Traza seeds the convention, Traza detects it, CI goes
//! green, and a real store answers `cause: null` forever. **A declared
//! convention may raise confidence here. It may never be a precondition.**
//!
//! The second refusal is about saying more than the data supports. Repetition
//! on its own is not evidence of anything: five identical spans are a retry
//! storm, a legitimate fan-out over five items, or a poll loop doing its job,
//! and the difference is not in the repetition. So a shape is classified only
//! when a discriminator agrees, and otherwise it is reported as
//! [`Shape::Inconclusive`] carrying what was missing. A detector that fires on
//! healthy traffic costs more than one that stays quiet, because the first
//! thing it burns is the reader's belief in the second finding.
//!
//! # The discriminators, and why these
//!
//! | Signal | Read from | Present when |
//! |---|---|---|
//! | error density | `Span::status` | always |
//! | serial fraction | span timestamps | always |
//! | context growth | [`semconv::LlmFacts::context_tokens`] | LLM spans reporting usage |
//! | self-similar depth | `parent_span_id` | always |
//! | declared retry | a `retry-of` link | ~never, so never required |
//!
//! Context growth is the one that earns its place. The dominant real agent
//! runaway is a model re-issuing a call with a conversation that grew by one
//! error message each turn, and it is invisible to input-identity comparison
//! (every attempt differs) while being obvious in the token counts — which
//! almost every pipeline reports, and which no content capture is needed to
//! read.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::semconv;
use crate::Span;

/// Repeats of one step within one trace before repetition is worth a look.
///
/// Two or three is ordinary — a retried call, a small fan-out. Five is not a
/// shape a correct program reaches by accident, and a discriminator still has
/// to agree before anything is called a fault.
const MIN_REPEATS: usize = 5;

/// Ancestors sharing a step's identity before the nesting itself is the
/// finding. A recursive decomposition three deep is a design; nine deep is a
/// runaway.
const MIN_SELF_DEPTH: usize = 5;

/// Fraction of consecutive pairs that must not overlap for a group to read as
/// serial. Retries wait for each other; a fan-out does not.
const SERIAL_FRACTION: f64 = 0.8;

/// Fraction of a group that must have failed for errors to be the story.
const ERROR_FRACTION: f64 = 0.5;

/// Context growth, as a multiple of the first call, that reads as runaway.
const GROWTH_FACTOR: u64 = 2;

/// Context growth, in absolute tokens, that reads as runaway however small
/// the multiple.
///
/// The ratio alone is scale-inverted in the direction that matters. An agent
/// carrying a 40,000-token system prompt and adding a 400-token tool result
/// each turn grows 8% over nine turns and burns 370,000 prompt tokens; an
/// agent starting at 1,500 doubles by turn four having burnt 25,000. The
/// expensive one is the one a ratio test never sees, so absolute growth is
/// tested beside it and either is enough.
const GROWTH_ABSOLUTE: u64 = 20_000;

/// Tolerance, in percent of the first value, within which context reads flat.
const FLAT_TOLERANCE_PERCENT: u64 = 10;

/// Members that must report context before a trend is claimed at all.
const MIN_TREND_SAMPLES: usize = 3;

/// Ancestors walked per span before the walk gives up.
///
/// The walk is guarded by a visited set, so this is not a cycle guard — it
/// bounds the honest case of a legitimately deep tree, keeping the pass
/// linear in the span count rather than quadratic in a pathological one.
const MAX_ANCESTOR_WALK: usize = 64;

/// How a session ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The run reached a successful terminal state.
    Success,
    /// The run ended on a failure.
    Failure,
    /// The run stopped without resolving.
    Abandoned,
    /// Nothing can be told. **Never rendered as success, and never counted in
    /// a success rate** — the whole point of naming it.
    Unknown,
}

/// Where an [`Outcome`] came from, which decides how much it is worth.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSource {
    /// A span declared it (see [`semconv::OUTCOME_KEYS`]).
    Declared,
    /// Derived from ordinary telemetry, because nobody declared anything.
    Derived,
    /// Not determinable.
    Unknown,
}

/// A session's terminal state, with its grounds.
#[derive(Clone, Debug, Serialize)]
pub struct SessionOutcome {
    /// What happened.
    pub outcome: Outcome,
    /// How that was decided.
    pub source: OutcomeSource,
    /// Why, in one machine-readable word: `declared`, `last_span_failed`,
    /// `last_span_succeeded`, `still_active`, or `no_spans`.
    pub reason: &'static str,
    /// The declared outcome text, verbatim, when a span declared one. This is
    /// producer text and is rendered as untrusted like any other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared: Option<String>,
    /// The declared goal, when a span carried one. Producer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Spans that failed, whatever the outcome. A run that recovered still
    /// says how much it had to recover from.
    pub error_count: usize,
    /// Spans considered.
    pub span_count: usize,
}

/// How a repeated step's context size moved across its attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenTrend {
    /// Non-decreasing, and materially larger by the end.
    Growing,
    /// Within [`FLAT_TOLERANCE_PERCENT`] end to end.
    Flat,
    /// Moving, but not monotonically upward.
    Varying,
    /// Too few members reported usage to say. **Not a synonym for flat** —
    /// this is the absence of evidence, and it can never satisfy a rule that
    /// wants evidence of health.
    Absent,
    /// Usage was reported under a caching convention whose arithmetic is not
    /// known, so any trend read off it would be a guess.
    Unknown,
}

/// What a repeated step turned out to be.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// Serial repeats, mostly failing.
    RetryStorm,
    /// Repeats whose context grows every attempt — work that cannot converge
    /// because its input is what keeps changing.
    ContextRunaway,
    /// A step nested inside itself past [`MIN_SELF_DEPTH`].
    SelfSimilarChain,
    /// Repeats the producer itself labelled retries.
    DeclaredRetry,
    /// Repetition that looks like ordinary work. Reported so a reader can see
    /// it was examined and set aside, and never eligible to be a cause.
    Iteration,
    /// Repetition no discriminator could explain. Carries what was missing.
    Inconclusive,
}

impl Shape {
    /// Whether this shape may be named as a run's cause. `Iteration` and
    /// `Inconclusive` may not: the first is a shape known to be ordinary, the
    /// second is an admission that nothing is known.
    pub fn is_fault(self) -> bool {
        !matches!(self, Self::Iteration | Self::Inconclusive)
    }
}

/// One span, named the way a caller can open it.
#[derive(Clone, Debug, Serialize)]
pub struct SpanRef {
    /// Trace holding the span.
    pub trace_id: String,
    /// The span.
    pub span_id: String,
}

/// A repeated step, with the evidence for calling it what it is called.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    /// What the repetition turned out to be.
    pub shape: Shape,
    /// Emitting service.
    pub service: String,
    /// Operation name.
    pub name: String,
    /// The trace the repetition happened in. Repetition is only ever counted
    /// WITHIN a trace: across a session, "the same step many times" is the
    /// definition of a conversation, not a fault.
    pub trace_id: String,
    /// Repeats observed.
    pub count: usize,
    /// How many failed.
    pub error_count: usize,
    /// Deepest nesting of this step inside itself.
    pub self_depth: usize,
    /// Whether the attempts waited for each other.
    pub serial_fraction: f64,
    /// How the context moved.
    pub token_trend: TokenTrend,
    /// Context tokens on the first and last attempt, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_first: Option<u64>,
    /// See [`Self::context_first`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_last: Option<u64>,
    /// A few spans that show it — first, last, and some between.
    pub spans: Vec<SpanRef>,
    /// For [`Shape::Inconclusive`], the signals that would have decided it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<&'static str>,
}

/// The step a run's failure is attributed to, and why that one.
#[derive(Clone, Debug, Serialize)]
pub struct Cause {
    /// The rule that named it: `leaf_error` or `repeated_fault`.
    pub rule: &'static str,
    /// How strong the grounds are: `declared`, `structural`, or `statistical`.
    pub confidence: &'static str,
    /// The step.
    pub span: SpanRef,
    /// Its operation name.
    pub name: String,
    /// Its service.
    pub service: String,
    /// Its status.
    pub status: String,
    /// One sentence a reader can check against the evidence beside it.
    pub because: String,
}

/// Everything the server can say about one run, in one document.
#[derive(Clone, Debug, Serialize)]
pub struct Diagnosis {
    /// How the run ended.
    pub outcome: SessionOutcome,
    /// The step the failure is attributed to, when one can be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// Repeated steps, faults first.
    pub findings: Vec<Finding>,
    /// Spans examined.
    pub examined: usize,
    /// Whether a cap stopped the analysis short. A truncated answer describes
    /// a prefix and says so — a bound that truncates in silence is a lie.
    pub truncated: bool,
}

/// The identity of a step: what makes two spans "the same thing again".
type Signature<'a> = (&'a str, &'a str);

fn signature(span: &Span) -> Signature<'_> {
    (span.service.as_str(), span.name.as_str())
}

fn is_error(span: &Span) -> bool {
    span.status.eq_ignore_ascii_case("error")
}

/// A session's terminal state.
///
/// Declared beats derived, but a declaration is only believed from the span
/// that ended last: an outcome is a statement about the run, and the newest
/// statement is the one that stands.
///
/// The derived rule reads the last span to END, and nothing else. That is
/// deliberately not "any error means failure": a run that failed a call,
/// retried it and finished is a SUCCESS with an error in it, and calling it a
/// failure would report every self-healing agent as broken. The error count
/// rides alongside so a recovered run still shows what it recovered from.
pub fn session_outcome(spans: &[Span], now_ns: u64, idle_ns: u64) -> SessionOutcome {
    let error_count = spans.iter().filter(|span| is_error(span)).count();
    let span_count = spans.len();
    let goal = spans.iter().find_map(|span| semconv::goal(&span.attributes));

    let Some(last) = spans.iter().max_by_key(|span| {
        (
            span.end_time_ns,
            span.trace_id.as_str(),
            span.span_id.as_str(),
        )
    }) else {
        return SessionOutcome {
            outcome: Outcome::Unknown,
            source: OutcomeSource::Unknown,
            reason: "no_spans",
            declared: None,
            goal,
            error_count,
            span_count,
        };
    };

    // A declaration, from whichever span declared one last.
    let declared = spans
        .iter()
        .filter_map(|span| {
            semconv::outcome(&span.attributes).map(|text| {
                (
                    span.end_time_ns,
                    span.trace_id.as_str(),
                    span.span_id.as_str(),
                    text,
                )
            })
        })
        .max_by(|left, right| (left.0, left.1, left.2).cmp(&(right.0, right.1, right.2)));
    if let Some((.., text)) = declared {
        let outcome = match text.to_ascii_lowercase().as_str() {
            "success" | "succeeded" | "ok" | "resolved" => Outcome::Success,
            "failure" | "failed" | "error" => Outcome::Failure,
            "abandoned" | "cancelled" | "canceled" | "timeout" => Outcome::Abandoned,
            _ => Outcome::Unknown,
        };
        return SessionOutcome {
            outcome,
            source: OutcomeSource::Declared,
            reason: "declared",
            declared: Some(text),
            goal,
            error_count,
            span_count,
        };
    }

    // Still running: not a success, and not a failure either.
    if now_ns.saturating_sub(last.end_time_ns) < idle_ns {
        return SessionOutcome {
            outcome: Outcome::Unknown,
            source: OutcomeSource::Derived,
            reason: "still_active",
            declared: None,
            goal,
            error_count,
            span_count,
        };
    }

    let failed = is_error(last);
    SessionOutcome {
        outcome: if failed {
            Outcome::Failure
        } else {
            Outcome::Success
        },
        source: OutcomeSource::Derived,
        reason: if failed {
            "last_span_failed"
        } else {
            "last_span_succeeded"
        },
        declared: None,
        goal,
        error_count,
        span_count,
    }
}

/// How a group's context size moved, or why that cannot be said.
fn token_trend(contexts: &[Option<u64>]) -> (TokenTrend, Option<u64>, Option<u64>) {
    // A single unreadable member poisons the trend rather than being skipped:
    // a caching convention we cannot do arithmetic on is not a gap in the
    // data, it is a reason to distrust the arithmetic.
    if contexts.iter().any(Option::is_none) && contexts.iter().flatten().count() > 0 {
        let reported: Vec<u64> = contexts.iter().flatten().copied().collect();
        if reported.len() < MIN_TREND_SAMPLES {
            return (TokenTrend::Absent, None, None);
        }
    }
    let reported: Vec<u64> = contexts.iter().flatten().copied().collect();
    if reported.len() < MIN_TREND_SAMPLES {
        return (TokenTrend::Absent, None, None);
    }
    let first = reported[0];
    let last = reported[reported.len() - 1];
    let monotonic = reported.windows(2).all(|pair| pair[1] >= pair[0]);
    let grown = last >= first.saturating_mul(GROWTH_FACTOR)
        || last.saturating_sub(first) >= GROWTH_ABSOLUTE;
    if monotonic && grown {
        return (TokenTrend::Growing, Some(first), Some(last));
    }
    let tolerance = first / 100 * FLAT_TOLERANCE_PERCENT;
    let spread = last.abs_diff(first);
    if spread <= tolerance {
        (TokenTrend::Flat, Some(first), Some(last))
    } else {
        (TokenTrend::Varying, Some(first), Some(last))
    }
}

/// Depth of each span inside a same-signature ancestry, keyed by span index.
///
/// The count is over ANCESTORS sharing the signature, not over an unbroken
/// run of them, because the shape this exists to catch alternates: a
/// think/act/observe agent never has a parent with its own name, so a
/// consecutive-run rule scores the canonical runaway as depth one.
fn self_depths(spans: &[Span]) -> Vec<usize> {
    // Parents resolve only within their own trace: span ids are unique inside
    // a trace, not across one session's traces, so a bare span_id map would
    // silently fuse two traces' forests.
    let mut index: HashMap<(&str, &str), usize> = HashMap::with_capacity(spans.len());
    for (position, span) in spans.iter().enumerate() {
        index.insert((span.trace_id.as_str(), span.span_id.as_str()), position);
    }
    let mut depths = vec![0_usize; spans.len()];
    for (position, span) in spans.iter().enumerate() {
        let mine = signature(span);
        let mut seen: HashSet<usize> = HashSet::new();
        let mut current = span;
        let mut depth = 0;
        for _ in 0..MAX_ANCESTOR_WALK {
            let Some(parent_id) = current
                .parent_span_id
                .as_deref()
                .filter(|parent| !parent.is_empty())
            else {
                break;
            };
            let Some(&parent) = index.get(&(current.trace_id.as_str(), parent_id)) else {
                break;
            };
            if !seen.insert(parent) {
                break;
            }
            if signature(&spans[parent]) == mine {
                depth += 1;
            }
            current = &spans[parent];
        }
        depths[position] = depth;
    }
    depths
}

/// Whether a group is spanned by links its producer labelled retries.
fn declared_retry(members: &[&Span]) -> bool {
    let addresses: HashSet<(&str, &str)> = members
        .iter()
        .map(|span| (span.trace_id.as_str(), span.span_id.as_str()))
        .collect();
    members
        .iter()
        .flat_map(|span| span.links.iter())
        .filter(|link| {
            link.attributes
                .get("relation")
                .and_then(Value::as_str)
                .is_some_and(|relation| relation == "retry-of")
        })
        .any(|link| addresses.contains(&(link.trace_id.as_str(), link.span_id.as_str())))
}

fn evidence_spans(members: &[&Span]) -> Vec<SpanRef> {
    const EVIDENCE: usize = 5;
    let mut refs: Vec<SpanRef> = Vec::new();
    for span in members.iter().take(EVIDENCE.saturating_sub(1)) {
        refs.push(SpanRef {
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
        });
    }
    if members.len() > EVIDENCE.saturating_sub(1) {
        if let Some(last) = members.last() {
            refs.push(SpanRef {
                trace_id: last.trace_id.clone(),
                span_id: last.span_id.clone(),
            });
        }
    }
    refs
}

/// Classifies one group of same-signature spans from one trace.
fn classify(members: &[&Span], max_self_depth: usize) -> Finding {
    let count = members.len();
    let error_count = members.iter().filter(|span| is_error(span)).count();
    let error_fraction = error_count as f64 / count as f64;

    // Serial means each attempt waited for the one before it. A fan-out
    // overlaps; retries do not.
    let mut ordered: Vec<&&Span> = members.iter().collect();
    ordered.sort_by_key(|span| span.start_time_ns);
    let pairs = ordered.len().saturating_sub(1);
    let serial = ordered
        .windows(2)
        .filter(|pair| pair[1].start_time_ns >= pair[0].end_time_ns)
        .count();
    let serial_fraction = if pairs == 0 {
        1.0
    } else {
        serial as f64 / pairs as f64
    };

    let contexts: Vec<Option<u64>> = ordered
        .iter()
        .map(|span| semconv::facts(&span.attributes).context_tokens())
        .collect();
    let reported = contexts.iter().flatten().count();
    let (trend, context_first, context_last) = token_trend(&contexts);
    let declared = declared_retry(members);

    let mut missing = Vec::new();
    let shape = if declared && count >= MIN_REPEATS {
        Shape::DeclaredRetry
    } else if max_self_depth >= MIN_SELF_DEPTH {
        Shape::SelfSimilarChain
    } else if count >= MIN_REPEATS && trend == TokenTrend::Growing {
        Shape::ContextRunaway
    } else if count >= MIN_REPEATS
        && serial_fraction >= SERIAL_FRACTION
        && error_fraction >= ERROR_FRACTION
    {
        Shape::RetryStorm
    } else if count >= MIN_REPEATS
        && (serial_fraction < SERIAL_FRACTION
            || (error_fraction < 0.1 && matches!(trend, TokenTrend::Flat | TokenTrend::Varying)))
    {
        // Ordinary work. Note the trend clause admits only POSITIVE evidence
        // of health: `Absent` and `Unknown` fall through to inconclusive,
        // because "we could not tell" must never be spendable as "it is fine".
        Shape::Iteration
    } else {
        if reported < MIN_TREND_SAMPLES {
            missing.push("context tokens on too few attempts to read a trend");
        }
        if trend == TokenTrend::Unknown {
            missing.push("a caching convention whose token arithmetic is not known");
        }
        if error_count == 0 {
            missing.push("no attempt failed");
        }
        Shape::Inconclusive
    };

    let first = members[0];
    Finding {
        shape,
        service: first.service.clone(),
        name: first.name.clone(),
        trace_id: first.trace_id.clone(),
        count,
        error_count,
        self_depth: max_self_depth,
        serial_fraction,
        token_trend: trend,
        context_first,
        context_last,
        spans: evidence_spans(&ordered.iter().map(|span| **span).collect::<Vec<_>>()),
        missing,
    }
}

/// The step a failure is attributed to.
///
/// The rule is the earliest LEAF error — an error span none of whose children
/// also errored. A parent that failed because its child failed is reporting
/// propagation, not cause, and naming it would point a reader at the top of
/// the tree every time. Ties break on `(start, trace, span)` so the answer is
/// stable across runs and across processes.
fn leaf_error_cause(spans: &[Span]) -> Option<&Span> {
    let mut errored_parents: HashSet<(&str, &str)> = HashSet::new();
    for span in spans.iter().filter(|span| is_error(span)) {
        if let Some(parent) = span
            .parent_span_id
            .as_deref()
            .filter(|parent| !parent.is_empty())
        {
            errored_parents.insert((span.trace_id.as_str(), parent));
        }
    }
    spans
        .iter()
        .filter(|span| is_error(span))
        .filter(|span| !errored_parents.contains(&(span.trace_id.as_str(), span.span_id.as_str())))
        .min_by_key(|span| {
            (
                span.start_time_ns,
                span.trace_id.as_str(),
                span.span_id.as_str(),
            )
        })
}

/// Diagnoses one run from the spans that make it up.
///
/// `spans` is whatever the caller resolved — a session or a trace — and the
/// whole answer is computed from that one slice, so every number in the
/// result describes the same instant. Reaching back into the store for a
/// second aggregate would let a seal land between the two and produce a
/// document whose own paragraphs disagree.
pub fn diagnose(spans: &[Span], now_ns: u64, idle_ns: u64, truncated: bool) -> Diagnosis {
    let outcome = session_outcome(spans, now_ns, idle_ns);
    let depths = self_depths(spans);

    // Repetition is counted WITHIN a trace. Across a session it is the
    // definition of a conversation: every multi-turn chat repeats one model
    // call with a context that grows by construction, and a detector keyed on
    // the session reports every healthy conversation as a runaway.
    let mut groups: HashMap<(&str, Signature<'_>), (Vec<&Span>, usize)> = HashMap::new();
    for (position, span) in spans.iter().enumerate() {
        let entry = groups
            .entry((span.trace_id.as_str(), signature(span)))
            .or_insert_with(|| (Vec::new(), 0));
        entry.0.push(span);
        entry.1 = entry.1.max(depths[position]);
    }

    let mut findings: Vec<Finding> = groups
        .into_values()
        .filter(|(members, depth)| members.len() >= MIN_REPEATS || *depth >= MIN_SELF_DEPTH)
        .map(|(members, depth)| classify(&members, depth))
        .collect();
    // Faults first, then the biggest repetition; the tail is stable by name so
    // two runs over one corpus agree.
    findings.sort_by(|left, right| {
        right
            .shape
            .is_fault()
            .cmp(&left.shape.is_fault())
            .then_with(|| right.count.cmp(&left.count))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.trace_id.cmp(&right.trace_id))
    });

    let cause = leaf_error_cause(spans).map(|span| {
        let fault = findings
            .iter()
            .find(|finding| finding.shape.is_fault() && finding.trace_id == span.trace_id);
        let (rule, confidence, because) = match fault {
            Some(finding) => (
                "repeated_fault",
                "structural",
                format!(
                    "it is the earliest step that failed without a failing child, and its \
                     operation repeats {} times in this trace ({} of them failing)",
                    finding.count, finding.error_count
                ),
            ),
            None => (
                "leaf_error",
                "statistical",
                "it is the earliest step that failed without a failing child, so the \
                 failures above it are propagation"
                    .to_owned(),
            ),
        };
        Cause {
            rule,
            confidence,
            span: SpanRef {
                trace_id: span.trace_id.clone(),
                span_id: span.span_id.clone(),
            },
            name: span.name.clone(),
            service: span.service.clone(),
            status: span.status.clone(),
            because,
        }
    });

    Diagnosis {
        outcome,
        cause,
        findings,
        examined: spans.len(),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(trace: &str, id: &str, parent: Option<&str>, name: &str, start: u64, end: u64) -> Span {
        serde_json::from_value(json!({
            "trace_id": trace, "span_id": id, "parent_span_id": parent,
            "name": name, "service": "agent",
            "start_time_ns": start, "end_time_ns": end,
        }))
        .expect("span")
    }

    fn erroring(mut span: Span) -> Span {
        span.status = "error".into();
        span
    }

    fn with_tokens(mut span: Span, prompt: u64) -> Span {
        span.attributes
            .insert("gen_ai.request.model".into(), json!("m"));
        span.attributes
            .insert("gen_ai.usage.input_tokens".into(), json!(prompt));
        span
    }

    const IDLE: u64 = 900_000_000_000;
    const NOW: u64 = 10_000_000_000_000;

    #[test]
    fn a_recovered_run_is_a_success_that_still_reports_its_errors() {
        let spans = vec![
            erroring(span("t", "a", None, "call", 1, 2)),
            span("t", "b", None, "call", 3, 4),
        ];
        let outcome = session_outcome(&spans, NOW, IDLE);
        assert_eq!(outcome.outcome, Outcome::Success);
        assert_eq!(outcome.reason, "last_span_succeeded");
        assert_eq!(
            outcome.error_count, 1,
            "the retry that healed it does not hide the failure it healed"
        );
    }

    #[test]
    fn a_still_running_session_is_unknown_not_successful() {
        let spans = vec![span("t", "a", None, "call", 1, NOW - 1)];
        let outcome = session_outcome(&spans, NOW, IDLE);
        assert_eq!(outcome.outcome, Outcome::Unknown);
        assert_eq!(outcome.reason, "still_active");
        assert_eq!(outcome.source, OutcomeSource::Derived);
    }

    #[test]
    fn an_alternating_agent_loop_is_seen_as_depth() {
        // think -> act -> think -> act ..., which a consecutive-run rule
        // scores as depth one because no span's parent shares its name.
        let mut spans = Vec::new();
        let mut parent: Option<String> = None;
        for turn in 0..12_u64 {
            let name = if turn % 2 == 0 { "reflect" } else { "search" };
            let id = format!("s{turn}");
            spans.push(span(
                "t",
                &id,
                parent.as_deref(),
                name,
                turn * 10,
                turn * 10 + 5,
            ));
            parent = Some(id);
        }
        let depths = self_depths(&spans);
        assert!(
            depths.iter().copied().max().unwrap_or(0) >= MIN_SELF_DEPTH,
            "alternating ancestry is still self-similar: {depths:?}"
        );
    }

    #[test]
    fn absent_usage_never_reads_as_healthy_iteration() {
        // Serial, no errors, no token data at all: the honest answer is that
        // nothing is known, not that the work is fine.
        let spans: Vec<Span> = (0..8_u64)
            .map(|turn| {
                span(
                    "t",
                    &format!("s{turn}"),
                    Some("root"),
                    "step",
                    turn * 100,
                    turn * 100 + 50,
                )
            })
            .collect();
        let members: Vec<&Span> = spans.iter().collect();
        let finding = classify(&members, 0);
        assert_eq!(finding.token_trend, TokenTrend::Absent);
        assert_eq!(
            finding.shape,
            Shape::Inconclusive,
            "absence of evidence is not evidence of health"
        );
        assert!(!finding.missing.is_empty(), "and it says what it lacked");
    }

    #[test]
    fn a_growing_context_repeat_is_a_runaway_and_a_flat_one_is_not() {
        let growing: Vec<Span> = (0..8_u64)
            .map(|turn| {
                with_tokens(
                    span(
                        "t",
                        &format!("s{turn}"),
                        Some("root"),
                        "step",
                        turn * 100,
                        turn * 100 + 50,
                    ),
                    1_000 + turn * 2_000,
                )
            })
            .collect();
        let members: Vec<&Span> = growing.iter().collect();
        assert_eq!(classify(&members, 0).shape, Shape::ContextRunaway);

        let flat: Vec<Span> = (0..8_u64)
            .map(|turn| {
                with_tokens(
                    span(
                        "t",
                        &format!("s{turn}"),
                        Some("root"),
                        "step",
                        turn * 100,
                        turn * 100 + 50,
                    ),
                    1_000,
                )
            })
            .collect();
        let members: Vec<&Span> = flat.iter().collect();
        assert_eq!(classify(&members, 0).shape, Shape::Iteration);
    }

    #[test]
    fn the_cause_is_the_earliest_leaf_error_not_the_parent_that_propagated_it() {
        let spans = vec![
            erroring(span("t", "root", None, "workflow", 0, 100)),
            erroring(span("t", "child", Some("root"), "tool", 10, 40)),
            erroring(span("t", "later", Some("root"), "tool", 50, 60)),
        ];
        let cause = leaf_error_cause(&spans).expect("a cause");
        assert_eq!(
            cause.span_id, "child",
            "the root only failed because its child did"
        );
    }

    #[test]
    fn a_conversation_is_not_a_runaway() {
        // Every turn is its own trace, which is what a multi-turn session
        // looks like, and prompt tokens grow by construction. Grouping within
        // a trace is what keeps this quiet.
        let spans: Vec<Span> = (0..20_u64)
            .map(|turn| {
                with_tokens(
                    span(
                        &format!("trace-{turn}"),
                        "s",
                        None,
                        "openai.chat",
                        turn * 1_000,
                        turn * 1_000 + 500,
                    ),
                    400 + turn * 500,
                )
            })
            .collect();
        let diagnosis = diagnose(&spans, NOW, IDLE, false);
        assert!(
            diagnosis.findings.is_empty(),
            "a healthy conversation produces no finding: {:?}",
            diagnosis.findings
        );
    }
}
