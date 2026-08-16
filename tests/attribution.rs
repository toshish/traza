//! Attribution held against the corpus it will actually meet.
//!
//! A detector is only as good as its silence. The interesting failure of a
//! loop finder is not that it misses a runaway — that is visible and gets
//! fixed — but that it fires on ordinary traffic, because the first thing a
//! false finding spends is the reader's belief in the next one.
//!
//! So the oracle here is the bundled seed corpus, which is a deliberate
//! spread of realistic agent workloads (multi-turn chats, tool-calling agents,
//! fan-outs, a failure-and-retry, RAG pipelines) and which contained no
//! runaway at all when this module was written. Every fault the analysis finds
//! in it is either a genuine one the corpus models or a bug in the analysis,
//! and the tests below pin which.

use std::collections::HashMap;

use traza::attribution::{diagnose, Outcome, Shape};
use traza::{seed, semconv, Span};

/// Fifteen minutes, the idle window a session is presumed still running in.
const IDLE_NS: u64 = 900_000_000_000;

fn corpus() -> Vec<Span> {
    seed::corpus(&seed::SeedOptions::default()).spans
}

/// Groups the corpus the way a session query would.
fn by_session(spans: &[Span]) -> HashMap<String, Vec<Span>> {
    let mut sessions: HashMap<String, Vec<Span>> = HashMap::new();
    for span in spans {
        if let Some(session) = semconv::facts(&span.attributes).session {
            sessions.entry(session).or_default().push(span.clone());
        }
    }
    sessions
}

/// The instant after the corpus ends, so nothing reads as still-running.
fn after(spans: &[Span]) -> u64 {
    spans
        .iter()
        .map(|span| span.end_time_ns)
        .max()
        .unwrap_or_default()
        + IDLE_NS * 2
}

#[test]
fn the_seeded_corpus_produces_no_phantom_runaways() {
    let spans = corpus();
    let now = after(&spans);
    let sessions = by_session(&spans);
    assert!(
        sessions.len() > 10,
        "the corpus should hold many sessions, found {}",
        sessions.len()
    );

    let mut faults: Vec<String> = Vec::new();
    for (session, members) in &sessions {
        let diagnosis = diagnose(members, now, IDLE_NS, false);
        for finding in &diagnosis.findings {
            if finding.shape.is_fault() && !session.starts_with("runaway-") {
                faults.push(format!(
                    "session {session}: {:?} on {}/{} count={} errors={} trend={:?} depth={}",
                    finding.shape,
                    finding.service,
                    finding.name,
                    finding.count,
                    finding.error_count,
                    finding.token_trend,
                    finding.self_depth
                ));
            }
        }
    }

    // Exactly one scenario in the corpus is a genuine fault, and it is the one
    // built to be one. Everything else is ordinary agent traffic and the
    // analysis must have nothing to say about it — the false-positive failure
    // mode is the one that matters, because it spends the reader's belief in
    // the finding that is real.
    assert!(
        faults.is_empty(),
        "the analysis found faults in healthy workloads:\n{}",
        faults.join("\n")
    );
}

#[test]
fn a_multi_turn_conversation_is_never_a_context_runaway() {
    // The seeded `multi_turn_session` grows its prompt tokens every turn, by
    // construction — that is what a conversation IS. An earlier design keyed
    // repetition on the session and reported every one of these as a runaway.
    let spans = corpus();
    let now = after(&spans);
    let sessions = by_session(&spans);
    let threads: Vec<_> = sessions
        .iter()
        .filter(|(session, _)| session.starts_with("thread-"))
        .collect();
    assert!(
        !threads.is_empty(),
        "the corpus should hold multi-turn threads"
    );

    for (session, members) in threads {
        let diagnosis = diagnose(members, now, IDLE_NS, false);
        let growing: Vec<_> = diagnosis
            .findings
            .iter()
            .filter(|finding| finding.shape == Shape::ContextRunaway)
            .collect();
        assert!(
            growing.is_empty(),
            "session {session} is an ordinary conversation: {growing:?}"
        );
    }
}

#[test]
fn the_seeded_retry_reads_as_a_recovered_run_not_a_failed_one() {
    // `failure_and_retry` fails a call and then succeeds. A session that
    // healed itself is a success that had an error in it, and reporting it as
    // a failure would call every resilient agent broken.
    let spans = corpus();
    let now = after(&spans);
    let mut recovered = 0;
    for members in by_session(&spans).values() {
        let diagnosis = diagnose(members, now, IDLE_NS, false);
        if diagnosis.outcome.error_count > 0 && diagnosis.outcome.outcome == Outcome::Success {
            recovered += 1;
            assert_eq!(diagnosis.outcome.reason, "last_span_succeeded");
        }
    }
    assert!(
        recovered > 0,
        "the corpus models at least one run that failed and recovered"
    );
}

#[test]
fn every_session_reports_an_outcome_and_never_invents_a_success() {
    let spans = corpus();
    let now = after(&spans);
    for (session, members) in by_session(&spans) {
        let diagnosis = diagnose(&members, now, IDLE_NS, false);
        // No pipeline emits Traza's outcome key, so the whole corpus must be
        // answerable without one. This is the property that keeps the feature
        // from being a closed loop over a convention Traza invented.
        assert_eq!(
            diagnosis.outcome.source,
            traza::attribution::OutcomeSource::Derived,
            "session {session} has no declared outcome and must still be answered"
        );
        assert_ne!(diagnosis.outcome.outcome, Outcome::Unknown);
    }
}

#[test]
fn the_seeded_runaway_is_found_and_its_failing_step_named() {
    // The scenario carries no marker saying it is the runaway: no declared
    // outcome, no retry link, nothing but ordinary OpenLLMetry attributes.
    // Everything asserted here has to be derived from that.
    let spans = corpus();
    let now = after(&spans);
    let sessions = by_session(&spans);
    let (session, members) = sessions
        .iter()
        .find(|(session, _)| session.starts_with("runaway-"))
        .expect("the corpus seeds a runaway");
    let diagnosis = diagnose(members, now, IDLE_NS, false);

    assert_eq!(
        diagnosis.outcome.outcome,
        Outcome::Failure,
        "session {session} ends on a failure"
    );

    // The reflection loop is found by context growth, which needs no content
    // capture — only the token counts every pipeline reports.
    let runaway = diagnosis
        .findings
        .iter()
        .find(|finding| finding.shape == Shape::ContextRunaway)
        .unwrap_or_else(|| panic!("a context runaway: {:#?}", diagnosis.findings));
    assert_eq!(runaway.name, "agent.reflect");
    assert!(runaway.count >= 5, "{runaway:?}");
    assert!(
        runaway.context_last.unwrap_or(0) > runaway.context_first.unwrap_or(0) * 2,
        "the context more than doubled across the loop: {runaway:?}"
    );

    // The failing tool is found by failure density on the same trace.
    let storm = diagnosis
        .findings
        .iter()
        .find(|finding| finding.shape == Shape::RetryStorm)
        .unwrap_or_else(|| panic!("a retry storm: {:#?}", diagnosis.findings));
    assert_eq!(storm.name, "tool.web_search");

    // The cause is the first search that failed — not the workflow root that
    // merely inherited its failure, and not the last one.
    let cause = diagnosis.cause.as_ref().expect("a cause");
    assert_eq!(cause.name, "tool.web_search", "{cause:?}");
    let expected = members
        .iter()
        .filter(|span| span.status == "error" && span.name == "tool.web_search")
        .min_by_key(|span| span.start_time_ns)
        .expect("a failing search");
    assert_eq!(
        cause.span.span_id, expected.span_id,
        "the earliest failing leaf is the cause"
    );
    assert!(
        cause.because.contains("without a failing child"),
        "and it says why: {}",
        cause.because
    );
}
