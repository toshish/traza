//! Recall: earlier runs that dealt with the same thing, successful ones first.
//!
//! An agent about to start a task can ask "has this been done before, and how
//! did it go?". Recall answers from the store's own content index: the spans
//! whose text contains every word of the query (whole-word AND, exactly the
//! `content=` search), grouped into the runs they belong to
//! ([`crate::semconv::run_id`]), each run summarised by how it ended, what it
//! cost, and a SKELETON — its step signatures in order, consecutive repeats
//! folded — that says what it did without quoting what it said.
//!
//! Ranking puts the runs a caller can learn from first: under `outcome=any`,
//! successful runs before every other outcome; then the runs with more
//! matching spans; then the most recent. `outcome=success` or `failure`
//! returns only runs that ended that way.
//!
//! Bounded at every stage, under one compute budget: the newest
//! [`MAX_RECALL_MATCHES`] matching spans are grouped, the
//! [`MAX_RECALL_CANDIDATES`] runs with the most matches among them are read in
//! full, and the result is cut to the caller's limit. A query whose matches
//! overflow the first bound says so (`truncated`).

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;

use crate::attribution::{self, Outcome, OutcomeSource};
use crate::{semconv, Deadline, Result, Span, SpanFilter, SpanSort, Store};

/// Runs returned when the caller names no limit.
pub const DEFAULT_RECALL_LIMIT: usize = 10;

/// Runs returned at most.
pub const MAX_RECALL_LIMIT: usize = 50;

/// Matching spans grouped into runs, newest first.
pub const MAX_RECALL_MATCHES: usize = 5000;

/// Runs read in full to be ranked. A run is a session or trace resolution,
/// which is the expensive part; this keeps a broad query to a bounded number
/// of them.
pub const MAX_RECALL_CANDIDATES: usize = 100;

/// Spans read per candidate run.
const MAX_RECALL_RUN_SPANS: usize = 20_000;

/// Entries in a skeleton, at most.
pub const MAX_SKELETON: usize = 40;

/// Which runs recall returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeFilter {
    /// Only runs that succeeded.
    Success,
    /// Only runs that failed.
    Failure,
    /// Every run, successes ranked first.
    Any,
}

impl OutcomeFilter {
    /// Parses `success`, `failure` or `any`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    fn admits(self, outcome: Outcome) -> bool {
        match self {
            Self::Success => outcome == Outcome::Success,
            Self::Failure => outcome == Outcome::Failure,
            Self::Any => true,
        }
    }
}

/// The step a failed run's failure is attributed to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecallCause {
    /// The step's trace.
    pub trace_id: String,
    /// The step's span.
    pub span_id: String,
    /// Its operation name.
    pub name: String,
    /// The attribution rule that named it (`leaf_error`, `repeated_fault`).
    pub rule: String,
}

/// One recalled run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecalledRun {
    /// The run id.
    pub run: String,
    /// The run's tenant (`""` is the default tenant) — the run id alone is
    /// ambiguous in an operator view, where two tenants may reuse one.
    pub tenant: String,
    /// How it ended.
    pub outcome: Outcome,
    /// Whether that was declared by a span or derived from the run.
    pub outcome_source: OutcomeSource,
    /// The declared goal, when a span carried one. Producer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// The run's spans that match the query, among those grouped.
    pub score: usize,
    /// The latest end time across the run.
    pub last_end_ns: u64,
    /// Steps (llm, tool and agent spans).
    pub steps: usize,
    /// Cost in USD over every span of the run, metered or priced.
    pub cost_usd: f64,
    /// Step signatures in order, consecutive repeats folded as `sig x3`.
    pub skeleton: Vec<String>,
    /// Whether the skeleton was cut to [`MAX_SKELETON`] entries.
    pub skeleton_truncated: bool,
    /// For a failed run, the step its failure is attributed to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<RecallCause>,
}

/// A recall answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Recall {
    /// The runs, ranked.
    pub runs: Vec<RecalledRun>,
    /// Whether the query matched more than [`MAX_RECALL_MATCHES`] spans, so
    /// that only the newest were grouped into runs.
    pub truncated: bool,
}

/// One candidate run while grouping matches.
struct Candidate {
    tenant: String,
    run: String,
    matches: usize,
    latest_start: u64,
}

impl Store {
    /// Runs whose spans contain every word of `query`, under `tenant` (the
    /// scoping rules of [`Store::get_run_in`]), ranked as the module describes
    /// and cut to `limit` (at most [`MAX_RECALL_LIMIT`]). `now_ns` dates the
    /// outcome rule.
    ///
    /// A query with no searchable word (ASCII letters or digits — see
    /// [`crate::content`]) is [`crate::Error::Invalid`]: it could match
    /// nothing, and would scan the whole store to find that out.
    pub fn recall(
        &self,
        tenant: Option<&str>,
        query: &str,
        outcome: OutcomeFilter,
        limit: usize,
        now_ns: u64,
    ) -> Result<Recall> {
        if crate::content::tokens(query).is_empty() {
            return Err(crate::Error::Invalid(
                "the query has no searchable words; recall matches whole ASCII words \
                 (letters and digits)"
                    .to_owned(),
            ));
        }
        let limit = limit.clamp(1, MAX_RECALL_LIMIT);
        let deadline = self.query_deadline(Instant::now());
        let filter = SpanFilter {
            content: Some(query.to_owned()),
            tenant: tenant.map(str::to_owned),
            sort: Some(SpanSort::StartDesc),
            limit: Some(MAX_RECALL_MATCHES + 1),
            ..SpanFilter::default()
        };
        let mut matches = self.query_under(&filter, deadline)?;
        let truncated = matches.len() > MAX_RECALL_MATCHES;
        matches.truncate(MAX_RECALL_MATCHES);

        let mut runs: HashMap<(String, String), Candidate> = HashMap::new();
        for span in &matches {
            let run = semconv::run_id(&span.attributes, &span.trace_id);
            let candidate = runs
                .entry((span.tenant.clone(), run.clone()))
                .or_insert_with(|| Candidate {
                    tenant: span.tenant.clone(),
                    run,
                    matches: 0,
                    latest_start: 0,
                });
            candidate.matches += 1;
            candidate.latest_start = candidate.latest_start.max(span.start_time_ns);
        }
        let mut candidates: Vec<Candidate> = runs.into_values().collect();
        candidates.sort_by(|left, right| {
            right
                .matches
                .cmp(&left.matches)
                .then(right.latest_start.cmp(&left.latest_start))
                .then_with(|| left.tenant.cmp(&right.tenant))
                .then_with(|| left.run.cmp(&right.run))
        });
        candidates.truncate(MAX_RECALL_CANDIDATES);

        let mut recalled = Vec::new();
        for candidate in candidates {
            if let Some(run) = self.recalled_run(candidate, outcome, now_ns, deadline)? {
                recalled.push(run);
            }
        }
        recalled.sort_by(|left, right| {
            let preferred = |run: &RecalledRun| {
                outcome == OutcomeFilter::Any && run.outcome == Outcome::Success
            };
            preferred(right)
                .cmp(&preferred(left))
                .then(right.score.cmp(&left.score))
                .then(right.last_end_ns.cmp(&left.last_end_ns))
                .then_with(|| left.tenant.cmp(&right.tenant))
                .then_with(|| left.run.cmp(&right.run))
        });
        recalled.truncate(limit);
        Ok(Recall {
            runs: recalled,
            truncated,
        })
    }

    /// Reads one candidate run in full and summarises it, or `None` when it
    /// does not pass the outcome filter (or vanished — an erasure can land
    /// between the match and the read).
    fn recalled_run(
        &self,
        candidate: Candidate,
        filter: OutcomeFilter,
        now_ns: u64,
        deadline: Option<Deadline>,
    ) -> Result<Option<RecalledRun>> {
        // Each run is read within its OWN tenant, even in the operator view:
        // two tenants reusing a session id are two runs, not one.
        let tape = self.run_tape(
            Some(&candidate.tenant),
            &candidate.run,
            MAX_RECALL_RUN_SPANS,
            usize::MAX,
            deadline,
        )?;
        if tape.spans.is_empty() {
            return Ok(None);
        }
        let ended = attribution::session_outcome(&tape.spans, now_ns, crate::mcp::SESSION_IDLE_NS);
        if !filter.admits(ended.outcome) {
            return Ok(None);
        }
        let cause = if ended.outcome == Outcome::Failure {
            attribution::diagnose(
                &tape.spans,
                now_ns,
                crate::mcp::SESSION_IDLE_NS,
                MAX_RECALL_RUN_SPANS,
            )
            .cause
            .map(|cause| RecallCause {
                trace_id: cause.span.trace_id,
                span_id: cause.span.span_id,
                name: cause.name,
                rule: cause.rule.to_owned(),
            })
        } else {
            None
        };
        let (skeleton, skeleton_truncated) =
            skeleton(tape.steps.iter().map(|step| step.detail.signature.as_str()));
        let (cost_usd, _) = crate::diff::run_cost(self, &tape.spans);
        Ok(Some(RecalledRun {
            run: candidate.run,
            tenant: candidate.tenant,
            outcome: ended.outcome,
            outcome_source: ended.source,
            goal: ended.goal,
            score: candidate.matches,
            last_end_ns: tape
                .spans
                .iter()
                .map(|span| span.end_time_ns)
                .max()
                .unwrap_or(0),
            steps: tape.steps.len(),
            cost_usd,
            skeleton,
            skeleton_truncated,
            cause,
        }))
    }

    /// A filtered query under a caller-anchored budget, so recall's content
    /// search and its run reads spend one budget between them.
    fn query_under(&self, filter: &SpanFilter, deadline: Option<Deadline>) -> Result<Vec<Span>> {
        let mask = self.erasure_mask();
        // Lock order: writer before segments (see Store field docs). Both are
        // released before any run is read, which takes them again.
        let writer = self.lock_writer()?;
        let segments = self.lock_segments()?;
        crate::query_view(
            &writer,
            &segments,
            &self.metrics,
            self.pricing(),
            filter,
            None,
            mask.as_deref(),
            deadline,
        )
    }
}

/// Run-length-compressed signatures: `["llm:gpt-4o", "tool:search x3", …]`,
/// at most [`MAX_SKELETON`] entries, and whether more were cut.
pub(crate) fn skeleton<'a>(signatures: impl Iterator<Item = &'a str>) -> (Vec<String>, bool) {
    let mut runs: Vec<(&str, usize)> = Vec::new();
    for signature in signatures {
        match runs.last_mut() {
            Some((last, count)) if *last == signature => *count += 1,
            _ => runs.push((signature, 1)),
        }
    }
    let truncated = runs.len() > MAX_SKELETON;
    let entries = runs
        .into_iter()
        .take(MAX_SKELETON)
        .map(|(signature, count)| match count {
            1 => signature.to_owned(),
            _ => format!("{signature} x{count}"),
        })
        .collect();
    (entries, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::{span, TempStore};
    use serde_json::{json, Value};

    const NOW: u64 = 1_700_000_000_000_000_000;
    const DAY: u64 = 86_400_000_000_000;

    #[test]
    fn skeletons_fold_repeats_and_cap_their_length() {
        let (entries, truncated) = skeleton(
            [
                "llm:m",
                "tool:search",
                "tool:search",
                "tool:search",
                "llm:m",
            ]
            .into_iter(),
        );
        assert_eq!(entries, ["llm:m", "tool:search x3", "llm:m"]);
        assert!(!truncated);
        let alternating: Vec<&str> = (0..100)
            .map(|index| if index % 2 == 0 { "llm:m" } else { "tool:t" })
            .collect();
        let (entries, truncated) = skeleton(alternating.into_iter());
        assert_eq!(entries.len(), MAX_SKELETON);
        assert!(truncated);
    }

    fn run_span(session: &str, id: &str, start: u64, status: &str, attributes: Value) -> Span {
        let mut attributes = attributes;
        attributes["session.id"] = json!(session);
        let mut span = span(session, id, start, attributes);
        span.status = status.into();
        span
    }

    #[test]
    fn successes_rank_first_and_the_filter_is_exact() {
        let fixture = TempStore::open("recall-rank", crate::Config::default());
        let store = fixture.store();
        let old = NOW - 3 * DAY;
        store
            .ingest_batch(vec![
                // A failed run with the MOST matches and the newest activity.
                run_span(
                    "fail-1",
                    "a",
                    old + 30,
                    "ok",
                    json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                           "gen_ai.prompt.0.content": "refund the order"}),
                ),
                run_span(
                    "fail-1",
                    "b",
                    old + 31,
                    "ok",
                    json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                           "gen_ai.prompt.0.content": "refund the order again"}),
                ),
                run_span(
                    "fail-1",
                    "c",
                    old + 32,
                    "error",
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "refund_api",
                           "gen_ai.tool.call.arguments": "{\"id\": 7}"}),
                ),
                // A successful run with one match, older.
                run_span(
                    "ok-1",
                    "a",
                    old + 10,
                    "ok",
                    json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                           "gen_ai.prompt.0.content": "please refund this order"}),
                ),
                run_span(
                    "ok-1",
                    "b",
                    old + 11,
                    "ok",
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "refund_api"}),
                ),
                run_span(
                    "ok-1",
                    "c",
                    old + 12,
                    "ok",
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "refund_api"}),
                ),
                // Unrelated.
                run_span(
                    "other",
                    "a",
                    old,
                    "ok",
                    json!({"gen_ai.operation.name": "chat",
                           "gen_ai.prompt.0.content": "weather report"}),
                ),
            ])
            .expect("ingests");

        let any = store
            .recall(Some(""), "refund order", OutcomeFilter::Any, 10, NOW)
            .expect("recalls");
        let ids: Vec<&str> = any.runs.iter().map(|run| run.run.as_str()).collect();
        assert_eq!(ids, ["ok-1", "fail-1"], "success outranks more matches");
        assert!(!any.truncated);
        let ok = &any.runs[0];
        assert_eq!(ok.outcome, Outcome::Success);
        assert_eq!(ok.score, 1);
        assert_eq!(ok.steps, 3);
        assert_eq!(ok.skeleton, ["llm:m", "tool:refund_api x2"]);
        assert!(ok.cause.is_none());
        let failed = &any.runs[1];
        assert_eq!(failed.score, 2);
        let cause = failed.cause.as_ref().expect("a failed run names its cause");
        assert_eq!(
            (cause.span_id.as_str(), cause.rule.as_str()),
            ("c", "leaf_error")
        );

        let only_failures = store
            .recall(Some(""), "refund order", OutcomeFilter::Failure, 10, NOW)
            .expect("recalls");
        assert_eq!(only_failures.runs.len(), 1);
        assert_eq!(only_failures.runs[0].run, "fail-1");
        let only_successes = store
            .recall(Some(""), "refund order", OutcomeFilter::Success, 10, NOW)
            .expect("recalls");
        assert_eq!(only_successes.runs.len(), 1);
        assert_eq!(only_successes.runs[0].run, "ok-1");

        let limited = store
            .recall(Some(""), "refund order", OutcomeFilter::Any, 1, NOW)
            .expect("recalls");
        assert_eq!(limited.runs.len(), 1);

        assert!(matches!(
            store.recall(Some(""), "?!", OutcomeFilter::Any, 10, NOW),
            Err(crate::Error::Invalid(_))
        ));
        assert!(store
            .recall(Some("acme"), "refund order", OutcomeFilter::Any, 10, NOW)
            .expect("recalls")
            .runs
            .is_empty());
    }

    #[test]
    fn a_run_still_active_is_unknown_and_never_ranked_as_success() {
        let fixture = TempStore::open("recall-active", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![run_span(
                "live",
                "a",
                NOW - 1_000,
                "ok",
                json!({"gen_ai.prompt.0.content": "refund", "gen_ai.operation.name": "chat"}),
            )])
            .expect("ingests");
        let recall = store
            .recall(Some(""), "refund", OutcomeFilter::Any, 10, NOW)
            .expect("recalls");
        assert_eq!(recall.runs[0].outcome, Outcome::Unknown);
        assert!(store
            .recall(Some(""), "refund", OutcomeFilter::Success, 10, NOW)
            .expect("recalls")
            .runs
            .is_empty());
    }
}
