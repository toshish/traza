//! The detector library: named TQL patterns for agent behaviours worth
//! knowing about, each with its threshold written into the pattern.
//!
//! Every detector is an ordinary query — `GET /v1/detectors` shows the
//! text, and pasting it into `/v1/trajectories/search` gives the same
//! answer — so a detector is never a black box, and a team that disagrees
//! with a threshold can run its own variant. The heuristics they lean on
//! (what counts as a test run, an edit, a test file) are documented in
//! `docs/guide/tql.md`.

use std::sync::OnceLock;

use serde::Serialize;

use super::Query;

/// How much a detector's finding usually matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The run's result is suspect: it may have passed by cheating, or
    /// called something that does not exist.
    High,
    /// The run wasted effort or lost the thread.
    Medium,
    /// Worth a look; often benign on its own.
    Low,
}

/// One named behaviour.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Detector {
    /// Stable identifier (`test_tampering`).
    pub name: &'static str,
    /// A short human title.
    pub title: &'static str,
    /// How much it usually matters.
    pub severity: Severity,
    /// What it finds, and its threshold, in a sentence.
    pub summary: &'static str,
    /// The TQL pattern.
    pub query: &'static str,
}

/// What `test_tampering` takes for a test file: a path under a
/// `test`/`tests`/`spec`/`__tests__` directory, or named `test_*`,
/// `*_test.*`, `*.test.*` or `*.spec.*`.
pub const TEST_FILE: &str = r#"(^|/)(tests?|spec|__tests__)/|(^|/)test_[^/]*$|_test\.[A-Za-z0-9]+$|\.(test|spec)\.[A-Za-z0-9]+$"#;

/// The library, in presentation order.
pub const DETECTORS: &[Detector] = &[
    Detector {
        name: "test_tampering",
        title: "Edited the tests to make them pass",
        severity: Severity::High,
        summary: "A test run fails, the agent edits a test file before any test passes, and \
                  a later test run passes.",
        query: concat!(
            "test[error] ..[not (test and not error)]> edit[file~\"",
            r#"(^|/)(tests?|spec|__tests__)/|(^|/)test_[^/]*$|_test\.[A-Za-z0-9]+$|\.(test|spec)\.[A-Za-z0-9]+$"#,
            "\"] ..> test[not error]"
        ),
    },
    Detector {
        name: "hallucinated_tool",
        title: "Called a tool that was not offered",
        severity: Severity::High,
        summary: "A tool step whose name is not in its requesting model call's tool catalog, or \
                  a model call asking for a tool outside its own catalog (only where the \
                  catalog was recorded).",
        query: "tool[in_catalog=false] | llm[unknown_calls]",
    },
    Detector {
        name: "give_up_loop",
        title: "Failed the same tool three times, then gave up",
        severity: Severity::Medium,
        summary: "The same tool fails at least 3 times with no success of it in between, and \
                  the run ends on a model answer without that tool ever succeeding.",
        query: "tool(@t)[error] (..[not (name=@t and not error)]> tool(@t)[error]){2,} \
                ..[not (name=@t and not error)]> llm$",
    },
    Detector {
        name: "edit_thrash",
        title: "Edited one file over and over",
        severity: Severity::Medium,
        summary: "The same file is edited at least 5 times within 10 steps.",
        query: "edit[file=@f] (..> edit[file=@f]){4,} within 10",
    },
    Detector {
        name: "ping_pong",
        title: "Agents handing work back and forth",
        severity: Severity::Medium,
        summary: "Agent A hands to agent B and back to A at least 3 times, with no other agent \
                  invoked in between.",
        query: "agent(@a) (..[kind!=agent]> agent(@b)[name!=@a] ..[kind!=agent]> agent(@a)){3,}",
    },
    Detector {
        name: "context_blowup",
        title: "Context doubled within a few steps",
        severity: Severity::Medium,
        summary: "A model call's context (at least 1000 tokens) is at least doubled by a model \
                  call no more than 5 steps later.",
        query: "llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6",
    },
    Detector {
        name: "retry_without_change",
        title: "Retried a failed call unchanged",
        severity: Severity::Low,
        summary: "A step fails and the identical request (same input digest) is sent again, \
                  with nothing but model calls in between.",
        query: "step[error, digest=@d] (..[kind=llm]> step[digest=@d]){1,}",
    },
    Detector {
        name: "tool_error_streak",
        title: "Five failing tool calls in a row",
        severity: Severity::Low,
        summary: "At least 5 tool calls fail with only model calls between them.",
        query: "tool[error] (..[kind=llm]> tool[error]){4,}",
    },
    Detector {
        name: "unverified_finish",
        title: "Finished without testing the last edit",
        severity: Severity::Low,
        summary: "The run's last edit is followed by no test run before the run ends on a \
                  model answer.",
        query: "edit ..[not test]> llm$",
    },
];

/// The detector named `name`.
pub fn detector(name: &str) -> Option<&'static Detector> {
    DETECTORS.iter().find(|detector| detector.name == name)
}

/// The library's patterns, compiled once.
pub(crate) fn compiled() -> &'static [Query] {
    static COMPILED: OnceLock<Vec<Query>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        DETECTORS
            .iter()
            .map(|detector| {
                Query::compile(detector.query).unwrap_or_else(|error| {
                    panic!(
                        "built-in detector {} does not compile: {error}",
                        detector.name
                    )
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::super::nfa::tests::reference_scan;
    use super::super::step::tests::{step_span, steps};
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn every_detector_compiles_and_the_test_file_regex_is_the_one_used() {
        assert_eq!(compiled().len(), DETECTORS.len());
        assert!(detector("test_tampering")
            .expect("exists")
            .query
            .contains(TEST_FILE));
        let names: Vec<_> = DETECTORS.iter().map(|d| d.name).collect();
        for wanted in [
            "test_tampering",
            "give_up_loop",
            "hallucinated_tool",
            "edit_thrash",
            "ping_pong",
            "retry_without_change",
            "context_blowup",
        ] {
            assert!(names.contains(&wanted), "{wanted} missing");
        }
    }

    fn bash(index: usize, command: &str, status: &str) -> crate::Span {
        step_span(
            index,
            "tool",
            "Bash",
            status,
            json!({"gen_ai.tool.call.arguments": {"command": command}}),
        )
    }

    fn edit(index: usize, file: &str) -> crate::Span {
        step_span(
            index,
            "tool",
            "Edit",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"file_path": file, "old_string": "a", "new_string": "b"}}),
        )
    }

    fn llm(index: usize, extra: Value) -> crate::Span {
        step_span(index, "llm", "claude", "ok", extra)
    }

    fn fires(name: &str, spans: &[crate::Span]) -> usize {
        let index = DETECTORS
            .iter()
            .position(|d| d.name == name)
            .expect("detector");
        let query = &compiled()[index];
        let steps = steps(spans);
        let scan = query
            .program
            .scan(&steps, usize::MAX, usize::MAX, None)
            .expect("scans");
        assert_eq!(scan.found, reference_scan(query, &steps), "{name}");
        scan.count
    }

    #[test]
    fn test_tampering_needs_fail_then_test_edit_then_pass() {
        let tampered = vec![
            bash(0, "pytest -q", "error"),
            edit(1, "tests/test_login.py"),
            bash(2, "pytest -q", "ok"),
        ];
        assert_eq!(fires("test_tampering", &tampered), 1);
        // Fixing the code instead is not tampering.
        let honest = vec![
            bash(0, "pytest -q", "error"),
            edit(1, "src/login.py"),
            bash(2, "pytest -q", "ok"),
        ];
        assert_eq!(fires("test_tampering", &honest), 0);
        // A test edit after a pass is not tampering with a failure.
        let later = vec![
            bash(0, "pytest -q", "error"),
            bash(1, "pytest -q", "ok"),
            edit(2, "tests/test_login.py"),
            bash(3, "pytest -q", "ok"),
        ];
        assert_eq!(fires("test_tampering", &later), 0);
        // Mutation guard: without the final pass it does not fire.
        assert_eq!(fires("test_tampering", &tampered[..2]), 0);
    }

    #[test]
    fn give_up_loop_needs_three_failures_and_a_final_answer() {
        let gave_up = vec![
            bash(0, "make", "error"),
            llm(1, json!({})),
            bash(2, "make", "error"),
            llm(3, json!({})),
            bash(4, "make", "error"),
            llm(5, json!({})),
        ];
        assert_eq!(fires("give_up_loop", &gave_up), 1);
        assert_eq!(
            fires("give_up_loop", &gave_up[..5]),
            0,
            "two failures then an answer is not three"
        );
        let mut recovered = gave_up.clone();
        recovered.insert(5, bash(5, "make", "ok"));
        for (index, span) in recovered.iter_mut().enumerate() {
            span.span_id = format!("r{index}");
            span.start_time_ns = 1_000 + index as u64 * 1_000_000_000;
        }
        assert_eq!(
            fires("give_up_loop", &recovered),
            0,
            "a success breaks the loop"
        );
    }

    #[test]
    fn hallucinated_tool_needs_a_known_catalog() {
        let catalog = json!({"gen_ai.tool.definitions": [{"name": "Bash"}]});
        let spans = vec![
            llm(0, catalog.clone()),
            bash(1, "ls", "ok"),
            step_span(2, "tool", "Teleport", "ok", json!({})),
        ];
        assert_eq!(fires("hallucinated_tool", &spans), 1);
        let unknown_catalog = vec![
            llm(0, json!({})),
            step_span(1, "tool", "Teleport", "ok", json!({})),
        ];
        assert_eq!(
            fires("hallucinated_tool", &unknown_catalog),
            0,
            "no catalog, no claim"
        );
    }

    #[test]
    fn edit_thrash_counts_edits_of_one_file_within_ten_steps() {
        let mut spans = Vec::new();
        for index in 0..5 {
            spans.push(edit(index * 2, "src/a.py"));
            spans.push(llm(index * 2 + 1, json!({})));
        }
        // Edits at 0, 2, 4, 6, 8: five within nine steps.
        assert_eq!(fires("edit_thrash", &spans), 1);
        let spread: Vec<_> = (0..5)
            .flat_map(|i| {
                vec![
                    edit(i * 3, "src/a.py"),
                    llm(i * 3 + 1, json!({})),
                    llm(i * 3 + 2, json!({})),
                ]
            })
            .collect();
        assert_eq!(
            fires("edit_thrash", &spread),
            0,
            "five edits over thirteen steps"
        );
    }

    #[test]
    fn ping_pong_counts_round_trips() {
        let agent = |index: usize, name: &str| step_span(index, "agent", name, "ok", json!({}));
        let spans: Vec<_> = ["a", "b", "a", "b", "a", "b", "a"]
            .iter()
            .enumerate()
            .map(|(index, name)| agent(index, name))
            .collect();
        assert_eq!(fires("ping_pong", &spans), 1);
        assert_eq!(fires("ping_pong", &spans[..5]), 0, "two round trips");
        let with_c: Vec<_> = ["a", "b", "a", "c", "a", "b", "a"]
            .iter()
            .enumerate()
            .map(|(index, name)| agent(index, name))
            .collect();
        assert_eq!(fires("ping_pong", &with_c), 0);
    }

    #[test]
    fn context_blowup_and_retry_and_streaks() {
        let context =
            |index: usize, tokens: u64| llm(index, json!({"gen_ai.usage.input_tokens": tokens}));
        assert_eq!(
            fires(
                "context_blowup",
                &[context(0, 2000), context(1, 2500), context(2, 4100)]
            ),
            1
        );
        assert_eq!(
            fires("context_blowup", &[context(0, 2000), context(1, 3900)]),
            0
        );
        assert_eq!(
            fires("context_blowup", &[context(0, 500), context(1, 1100)]),
            0,
            "below the floor"
        );
        let mut far = vec![context(0, 2000)];
        far.extend((1..7).map(|i| bash(i, "ls", "ok")));
        far.push(context(7, 5000));
        assert_eq!(
            fires("context_blowup", &far),
            0,
            "seven steps later is too far"
        );
        far.remove(1);
        for (index, span) in far.iter_mut().enumerate() {
            span.span_id = format!("f{index}");
            span.start_time_ns = 1_000 + index as u64 * 1_000_000_000;
        }
        assert_eq!(
            fires("context_blowup", &far),
            0,
            "six steps later is still too far"
        );
        far.remove(1);
        for (index, span) in far.iter_mut().enumerate() {
            span.span_id = format!("g{index}");
            span.start_time_ns = 1_000 + index as u64 * 1_000_000_000;
        }
        assert_eq!(
            fires("context_blowup", &far),
            1,
            "five steps later is in reach"
        );

        let retry = vec![
            bash(0, "curl x", "error"),
            llm(1, json!({})),
            bash(2, "curl x", "error"),
        ];
        assert_eq!(fires("retry_without_change", &retry), 1);
        let changed = vec![
            bash(0, "curl x", "error"),
            llm(1, json!({})),
            bash(2, "curl y", "ok"),
        ];
        assert_eq!(fires("retry_without_change", &changed), 0);

        let streak: Vec<_> = (0..5)
            .map(|i| bash(i, &format!("cmd {i}"), "error"))
            .collect();
        assert_eq!(fires("tool_error_streak", &streak), 1);
        assert_eq!(fires("tool_error_streak", &streak[..4]), 0);
    }

    #[test]
    fn unverified_finish_needs_no_test_after_the_last_edit() {
        let unverified = vec![
            bash(0, "pytest", "ok"),
            edit(1, "src/a.py"),
            llm(2, json!({})),
        ];
        assert_eq!(fires("unverified_finish", &unverified), 1);
        let verified = vec![
            edit(0, "src/a.py"),
            bash(1, "pytest", "ok"),
            llm(2, json!({})),
        ];
        assert_eq!(fires("unverified_finish", &verified), 0);
    }
}
