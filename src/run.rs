//! Runs and their steps: the store-level half of run identity.
//!
//! A run is one unit of agent work within a tenant. Its id is the span's
//! session id when it carries one, else `trace:<trace_id>`
//! ([`semconv::run_id`]), so a conversation spanning many traces is one run
//! and an uninstrumented trace is still a run of its own. This module resolves
//! a run id back to its spans — under the store's snapshot rules, erasure mask
//! and compute budget — and pairs a span's step facts with its PRICED LLM
//! facts, which only the store can do because only the store holds the
//! pricing table.

use std::cmp::Ordering;
use std::time::Instant;

use crate::semconv::{self, LlmFacts, StepFacts};
use crate::{Deadline, Error, Result, Span, Store};

/// A run's spans, in step order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSpans {
    /// The run's spans sorted by `(start_time_ns, trace_id, span_id)`, with the
    /// tenant as a final tie-break for an operator view spanning tenants.
    pub spans: Vec<Span>,
    /// Whether the run held more spans than the caller's maximum, in which
    /// case `spans` is its EARLIEST spans in that order and the rest are
    /// missing.
    pub truncated: bool,
}

impl Store {
    /// Every span of `run_id`, in step order, at most `max_spans` of them.
    ///
    /// `trace:<id>` resolves to that trace's spans; any other id resolves as a
    /// session, across every recognized session key, exactly as
    /// [`Store::session_in`] does. `tenant` scopes the run: `Some(tenant)` is
    /// that tenant's run alone, and `None` is the operator view across every
    /// tenant — a union, if two tenants reuse an id.
    ///
    /// Budgeted by [`crate::Config::query_deadline`], anchored here. The
    /// maximum bounds the RESULT, not the work: resolution reads the whole run
    /// before keeping its earliest spans, and the deadline is what bounds that.
    ///
    /// An empty id, or a bare `trace:` naming no trace, is
    /// [`Error::Invalid`]; an id no span carries is an empty run.
    pub fn get_run_in(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        max_spans: usize,
    ) -> Result<RunSpans> {
        let deadline = self.query_deadline(Instant::now());
        self.run_spans(tenant, run_id, max_spans, deadline)
    }

    /// [`Self::get_run_in`] under a caller-anchored budget, so a request that
    /// resolves several runs (a diff resolves two) spends ONE budget across
    /// all of them rather than one per run.
    pub(crate) fn run_spans(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        max_spans: usize,
        deadline: Option<Deadline>,
    ) -> Result<RunSpans> {
        let mut spans = match semconv::run_trace_id(run_id) {
            Some("") => {
                return Err(Error::Invalid(format!(
                    "run id `{run_id}` names no trace; expected `{}<trace_id>`",
                    semconv::TRACE_RUN_PREFIX
                )))
            }
            Some(trace_id) => self.trace_spans_in(tenant, trace_id, deadline)?,
            None if run_id.is_empty() => {
                return Err(Error::Invalid("run id is empty".to_owned()));
            }
            None => self.resolve_session_spans(tenant, run_id, deadline)?,
        };
        spans.sort_by(compare_steps);
        let truncated = spans.len() > max_spans;
        spans.truncate(max_spans);
        Ok(RunSpans { spans, truncated })
    }

    /// A span's step facts paired with its LLM facts, this store's pricing
    /// applied to the latter.
    ///
    /// The surfaces that report a step's cost must come through here rather
    /// than through [`semconv::step_facts`] plus [`semconv::facts`], for the
    /// reason [`semconv::LlmFacts::priced`] gives: a cost read without the
    /// pricing table is metered cost only, and disagrees with every surface
    /// that prices. Facts are extracted once and shared by both halves.
    pub fn step_facts(&self, span: &Span) -> (StepFacts, LlmFacts) {
        let llm = self.facts(span);
        (semconv::step_facts_with(span, &llm), llm)
    }
}

/// Step order: `(start_time_ns, trace_id, span_id)`, then tenant.
///
/// Not the store's general span order, which breaks start-time ties on END
/// time first: here two steps that begin together are ordered by identity, so
/// a step's position does not move when it is re-ingested with a later end
/// time, and a tape read mid-run and again after it agree on its prefix.
fn compare_steps(left: &Span, right: &Span) -> Ordering {
    left.start_time_ns
        .cmp(&right.start_time_ns)
        .then_with(|| left.trace_id.cmp(&right.trace_id))
        .then_with(|| left.span_id.cmp(&right.span_id))
        .then_with(|| left.tenant.cmp(&right.tenant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    struct TempStore {
        dir: std::path::PathBuf,
        store: Option<Store>,
    }

    impl TempStore {
        fn open(label: &str, config: crate::Config) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("traza-run-{label}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            let store = Store::open(&dir, config).expect("opens");
            Self {
                dir,
                store: Some(store),
            }
        }

        fn store(&self) -> &Store {
            self.store.as_ref().expect("open")
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            drop(self.store.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn span(tenant: &str, trace: &str, id: &str, start: u64, end: u64, attributes: Value) -> Span {
        Span {
            trace_id: trace.into(),
            span_id: id.into(),
            tenant: tenant.into(),
            parent_span_id: None,
            name: format!("op-{id}"),
            start_time_ns: start,
            end_time_ns: end,
            status: "ok".into(),
            service: "svc".into(),
            attributes: attributes.as_object().expect("object").clone(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn ids(run: &RunSpans) -> Vec<String> {
        run.spans
            .iter()
            .map(|span| format!("{}/{}", span.trace_id, span.span_id))
            .collect()
    }

    /// One session (`s1`) spread over three traces, two session keys, a
    /// sealed segment and the write buffer; a session-less span inside one of
    /// its traces; another session; and another tenant reusing both the trace
    /// id and the session id.
    fn seeded(label: &str) -> TempStore {
        let fixture = TempStore::open(label, crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                // `t1/s-a` and `t1/s-b` start together; `s-b` ENDS first, so
                // the store's general order would put it first. Step order
                // does not look at end times.
                span("", "t1", "s-b", 200, 250, json!({"session.id": "s1"})),
                span("", "t1", "s-a", 200, 900, json!({"session.id": "s1"})),
                span("", "t1", "s-c", 100, 150, json!({"http.method": "GET"})),
            ])
            .expect("ingests");
        store.flush().expect("seals the first batch into a segment");
        store
            .ingest_batch(vec![
                span(
                    "",
                    "t2",
                    "x1",
                    150,
                    160,
                    json!({"gen_ai.conversation.id": "s1"}),
                ),
                span("", "t2", "x2", 50, 60, json!({"session.id": "other"})),
                span("", "t0", "y1", 200, 210, json!({"session.id": "s1"})),
                span("acme", "t1", "s-z", 10, 20, json!({"session.id": "s1"})),
            ])
            .expect("ingests");
        fixture
    }

    #[test]
    fn a_trace_run_is_that_traces_spans_in_step_order() {
        let fixture = seeded("trace");
        let run = fixture
            .store()
            .get_run_in(Some(""), "trace:t1", 100)
            .expect("resolves");
        assert_eq!(ids(&run), ["t1/s-c", "t1/s-a", "t1/s-b"]);
        assert!(!run.truncated);

        // The operator view spans tenants: acme's same-id trace joins.
        let operator = fixture
            .store()
            .get_run_in(None, "trace:t1", 100)
            .expect("resolves");
        assert_eq!(ids(&operator), ["t1/s-z", "t1/s-c", "t1/s-a", "t1/s-b"]);
        assert_eq!(operator.spans[0].tenant, "acme");
    }

    #[test]
    fn a_session_run_unions_keys_and_traces_within_its_tenant() {
        let fixture = seeded("session");
        let run = fixture
            .store()
            .get_run_in(Some(""), "s1", 100)
            .expect("resolves");
        // Start order, then trace id (`t0` before `t1` at 200), then span id.
        // `t1/s-c` carries no session key and `t2/x2` another session, so
        // neither is in the run; acme's `s1` is another tenant's run.
        assert_eq!(ids(&run), ["t2/x1", "t0/y1", "t1/s-a", "t1/s-b"]);
        assert!(!run.truncated);

        let acme = fixture
            .store()
            .get_run_in(Some("acme"), "s1", 100)
            .expect("resolves");
        assert_eq!(ids(&acme), ["t1/s-z"]);
    }

    #[test]
    fn the_maximum_keeps_the_earliest_spans_and_says_so() {
        let fixture = seeded("truncate");
        let store = fixture.store();
        let run = store.get_run_in(Some(""), "s1", 2).expect("resolves");
        assert_eq!(ids(&run), ["t2/x1", "t0/y1"]);
        assert!(run.truncated);

        let exact = store.get_run_in(Some(""), "s1", 4).expect("resolves");
        assert_eq!(exact.spans.len(), 4);
        assert!(!exact.truncated, "a run that fits exactly is not truncated");

        let none = store.get_run_in(Some(""), "s1", 0).expect("resolves");
        assert!(none.spans.is_empty() && none.truncated);
    }

    #[test]
    fn an_unknown_run_is_empty_and_a_malformed_id_is_refused() {
        let fixture = seeded("malformed");
        let store = fixture.store();
        for unknown in ["no-such-session", "trace:no-such-trace"] {
            let run = store.get_run_in(Some(""), unknown, 10).expect("resolves");
            assert!(run.spans.is_empty() && !run.truncated, "{unknown}");
        }
        for (malformed, needle) in [("", "run id is empty"), ("trace:", "names no trace")] {
            match store.get_run_in(Some(""), malformed, 10) {
                Err(Error::Invalid(message)) => {
                    assert!(message.contains(needle), "{malformed:?}: {message}")
                }
                other => panic!("{malformed:?} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolution_spends_the_callers_budget() {
        let fixture = seeded("deadline");
        let store = fixture.store();
        // A budget of zero is exhausted on arrival, so every check refuses —
        // which proves both resolution paths consult the deadline they are
        // handed rather than anchoring their own.
        let expired = Deadline::starting(Instant::now(), Some(std::time::Duration::ZERO));
        for run_id in ["trace:t1", "s1"] {
            match store.run_spans(Some(""), run_id, 10, expired) {
                Err(Error::DeadlineExceeded(_)) => {}
                other => panic!("{run_id}: expected a deadline refusal, got {other:?}"),
            }
        }
        // And no deadline means no budget.
        let run = store
            .run_spans(Some(""), "s1", 10, None)
            .expect("unbounded");
        assert_eq!(run.spans.len(), 4);
    }

    #[test]
    fn store_step_facts_prices_what_semconv_alone_cannot() {
        let pricing = crate::pricing::Pricing::parse(
            r#"{"models": {"gpt-4o": {"input_per_mtok": 2.0, "output_per_mtok": 8.0}}}"#,
        )
        .expect("pricing parses");
        let config = crate::Config {
            pricing: std::sync::Arc::new(pricing),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("priced", config);
        let call = span(
            "",
            "t1",
            "llm",
            1,
            2,
            json!({
                "gen_ai.operation.name": "chat",
                "gen_ai.request.model": "gpt-4o",
                "gen_ai.usage.input_tokens": 1_000_000,
                "gen_ai.usage.output_tokens": 500_000,
                "gen_ai.input.messages": [{"role": "user", "content": "hi"}],
            }),
        );
        let (step, llm) = fixture.store().step_facts(&call);
        assert_eq!(llm.cost_usd, Some(6.0), "2.0 + 0.5 * 8.0 at list price");
        assert!(llm.cost_derived);
        assert!(
            semconv::facts(&call.attributes).cost_usd.is_none(),
            "extraction alone reports metered cost only"
        );
        assert_eq!(step, semconv::step_facts(&call));
        assert_eq!(step.kind, semconv::StepKind::Llm);
        assert_eq!(step.fingerprint, "llm:gpt-4o#4e79873118cd9be7");
    }
}
