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
    /// How much it matters in a run that succeeded, when that is less: a
    /// behaviour that wastes effort matters less once the run got there
    /// anyway. `None`: the outcome does not change it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity_if_succeeded: Option<Severity>,
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
        severity_if_succeeded: None,
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
        severity_if_succeeded: None,
        summary: "A tool step whose name is not in its requesting model call's tool catalog, or \
                  a model call asking for a tool outside its own catalog (only where the \
                  catalog was recorded).",
        query: "tool[in_catalog=false] | llm[unknown_calls]",
    },
    Detector {
        name: "give_up_loop",
        title: "Failed the same tool call three times, then gave up",
        severity: Severity::Medium,
        severity_if_succeeded: None,
        summary: "The same tool call — same tool, same input (its fingerprint) — fails at least \
                  3 times, that tool never succeeds in between or afterwards, and the run ends \
                  on a model answer.",
        query: "tool(@t)[error, fingerprint=@c] \
                (..[not (name=@t and not error)]> tool[error, fingerprint=@c]){2,} \
                ..[not (name=@t and not error)]> llm$",
    },
    Detector {
        name: "edit_thrash",
        title: "Edits of one file going back and forth",
        severity: Severity::Medium,
        severity_if_succeeded: Some(Severity::Low),
        summary: "Within 30 steps, one file's edits undo and redo each other — text an edit \
                  wrote is removed by a later edit and written again, or the file is rewritten \
                  whole back to an earlier version — or the file is edited three times with a \
                  failing test run after each and no passing one between. Medium; low in a run \
                  that succeeded.",
        query: concat!(
            "edit[file=@f, added=@x] ..> edit[file=@f, removed=@x] ..> edit[file=@f, added=@x]",
            " | edit[file=@f, added=@x, not removed] ..> edit[file=@f, added!=@x]",
            " ..> edit[file=@f, added=@x, not removed]",
            " | edit[file=@f] ..[not (test and not error)]> test[error]",
            " (..[not (test and not error)]> edit[file=@f] ..[not (test and not error)]> test[error]){2,}",
            " within 30"
        ),
    },
    Detector {
        name: "ping_pong",
        title: "Agents handing work back and forth",
        severity: Severity::Medium,
        severity_if_succeeded: None,
        summary: "Agent A hands to agent B and back to A at least 3 times, with no other agent \
                  invoked in between.",
        query: "agent(@a) (..[kind!=agent]> agent(@b)[name!=@a] ..[kind!=agent]> agent(@a)){3,}",
    },
    Detector {
        name: "context_blowup",
        title: "Context doubled within a few steps",
        severity: Severity::Medium,
        severity_if_succeeded: None,
        summary: "A model call's context (at least 1000 tokens) is at least doubled by a model \
                  call of the same conversation no more than 5 steps later — calls naming a \
                  conversation (gen_ai.conversation.id: a main thread, or one sub-agent) are \
                  compared within it, calls naming none within their agent, and calls naming \
                  neither with each other.",
        query: concat!(
            "llm[context>=1000, context=@c, conversation=@v]",
            " ..> llm[context>=2*@c, conversation=@v]",
            " | llm[context>=1000, context=@c, not conversation, agent=@a]",
            " ..> llm[context>=2*@c, not conversation, agent=@a]",
            " | llm[context>=1000, context=@c, not conversation, not agent]",
            " ..> llm[context>=2*@c, not conversation, not agent]",
            " within 6"
        ),
    },
    Detector {
        name: "retry_without_change",
        title: "Retried a failed call unchanged",
        severity: Severity::Low,
        severity_if_succeeded: None,
        summary: "A step fails and the identical request (same input digest) is sent again, \
                  with nothing but model calls in between.",
        query: "step[error, digest=@d] (..[kind=llm]> step[digest=@d]){1,}",
    },
    Detector {
        name: "tool_error_streak",
        title: "Five failing tool calls in a row",
        severity: Severity::Low,
        severity_if_succeeded: None,
        summary: "At least 5 tool calls fail with only model calls between them.",
        query: "tool[error] (..[kind=llm]> tool[error]){4,}",
    },
    Detector {
        name: "unverified_finish",
        title: "Finished without testing the last edit",
        severity: Severity::Low,
        severity_if_succeeded: None,
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
        // Three different failing commands through one tool are not one
        // call failing three times.
        let different = vec![
            bash(0, "ls missing", "error"),
            bash(1, "cat nope", "error"),
            bash(2, "make", "error"),
            llm(3, json!({})),
        ];
        assert_eq!(fires("give_up_loop", &different), 0);
        // The same call, with other failing calls of the tool between.
        let interleaved = vec![
            bash(0, "make", "error"),
            bash(1, "ls", "error"),
            bash(2, "make", "error"),
            bash(3, "make", "error"),
            llm(4, json!({})),
        ];
        assert_eq!(fires("give_up_loop", &interleaved), 1);
    }

    #[test]
    fn test_tampering_ignores_a_runner_named_outside_a_command_position() {
        // No test ever passed: installing a pytest plugin is not a test run.
        let spans = vec![
            bash(0, "pytest -q", "error"),
            edit(1, "tests/test_a.py"),
            bash(2, "pip install pytest-xdist", "ok"),
        ];
        assert_eq!(fires("test_tampering", &spans), 0);
        let mut passed = spans.clone();
        passed.push(bash(3, "python -m pytest -q", "ok"));
        assert_eq!(fires("test_tampering", &passed), 1);
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

    /// An `Edit` of `file` replacing `old` with `new`.
    fn change(index: usize, file: &str, old: &str, new: &str) -> crate::Span {
        step_span(
            index,
            "tool",
            "Edit",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"file_path": file, "old_string": old, "new_string": new}}),
        )
    }

    /// A whole-file `Write` of `file`.
    fn write(index: usize, file: &str, content: &str) -> crate::Span {
        step_span(
            index,
            "tool",
            "Write",
            "ok",
            json!({"gen_ai.tool.call.arguments": {"file_path": file, "content": content}}),
        )
    }

    /// `spans` renumbered in order, so a test can splice steps in.
    fn renumbered(mut spans: Vec<crate::Span>) -> Vec<crate::Span> {
        for (index, span) in spans.iter_mut().enumerate() {
            span.span_id = format!("n{index:04}");
            span.start_time_ns = 1_000 + index as u64 * 1_000_000_000;
        }
        spans
    }

    #[test]
    fn edit_thrash_is_not_merely_many_edits() {
        // Five edits of one file inside nine steps, each a step forward: a
        // feature being written, not thrash.
        let mut spans = Vec::new();
        for index in 0..5 {
            spans.push(change(
                index * 2,
                "src/a.py",
                &format!("step {index}"),
                &format!("step {}", index + 1),
            ));
            spans.push(llm(index * 2 + 1, json!({})));
        }
        assert_eq!(fires("edit_thrash", &spans), 0, "progress is not thrash");
        // The same five edits of the old detector's fixture: still not.
        let same: Vec<_> = (0..5)
            .flat_map(|i| vec![edit(i * 2, "src/a.py"), llm(i * 2 + 1, json!({}))])
            .collect();
        assert_eq!(fires("edit_thrash", &same), 0);
    }

    #[test]
    fn edit_thrash_finds_an_edit_undone_and_redone() {
        let thrash = renumbered(vec![
            change(0, "src/a.py", "return x", "return x + 1"),
            llm(1, json!({})),
            // The undo re-indents: normalised text still matches.
            change(2, "src/a.py", "return  x + 1", "  return x"),
            llm(3, json!({})),
            change(4, "src/a.py", "return x", "return x + 1"),
        ]);
        assert_eq!(fires("edit_thrash", &thrash), 1, "A→B, B→A, A→B");
        // One revert is a change of mind (or a debug print removed), not
        // thrash.
        assert_eq!(fires("edit_thrash", &thrash[..4]), 0);
        // Reverting twice to the same text: two attempts both undone.
        let attempts = renumbered(vec![
            change(0, "src/a.py", "a = 1", "a = 2"),
            change(1, "src/a.py", "a = 2", "a = 1"),
            change(2, "src/a.py", "a = 1", "a = 3"),
            change(3, "src/a.py", "a = 3", "a = 1"),
        ]);
        assert_eq!(fires("edit_thrash", &attempts), 1);
        // The undo of another file's edit is not this file's.
        let elsewhere = renumbered(vec![
            change(0, "src/a.py", "return x", "return x + 1"),
            change(1, "src/b.py", "return x + 1", "return x"),
            change(2, "src/a.py", "return x", "return x + 1"),
        ]);
        assert_eq!(fires("edit_thrash", &elsewhere), 0);
        // Codex patches undo and redo the same way.
        let patch = |index: usize, minus: &str, plus: &str| {
            step_span(
                index,
                "tool",
                "apply_patch",
                "ok",
                json!({"gen_ai.tool.call.arguments": {"input": format!(
                    "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-{minus}\n+{plus}\n*** End Patch"
                )}}),
            )
        };
        let codex = vec![
            patch(0, "let n = 1;", "let n = 2;"),
            patch(1, "let n = 2;", "let n = 1;"),
            patch(2, "let n = 1;", "let n = 2;"),
        ];
        assert_eq!(fires("edit_thrash", &codex), 1);
    }

    #[test]
    fn edit_thrash_finds_a_file_rewritten_back_to_an_earlier_version() {
        let back = vec![
            write(0, "src/a.py", "v1\n"),
            write(1, "src/a.py", "v2\n"),
            write(2, "src/a.py", "v1\n"),
        ];
        assert_eq!(fires("edit_thrash", &back), 1, "v1, v2, v1");
        let forward = vec![
            write(0, "src/a.py", "v1\n"),
            write(1, "src/a.py", "v2\n"),
            write(2, "src/a.py", "v3\n"),
        ];
        assert_eq!(fires("edit_thrash", &forward), 0);
        // Rewritten whole, the same file again, an edit between: undone.
        let undone = vec![
            write(0, "src/a.py", "v1\n"),
            change(1, "src/a.py", "v1", "v1 tweaked"),
            write(2, "src/a.py", "v1\n"),
        ];
        assert_eq!(fires("edit_thrash", &undone), 1);
        // Writing the same content twice in a row changes nothing.
        let twice = vec![write(0, "src/a.py", "v1\n"), write(1, "src/a.py", "v1\n")];
        assert_eq!(fires("edit_thrash", &twice), 0);
    }

    #[test]
    fn edit_thrash_finds_edits_that_keep_failing_the_tests() {
        let round = |at: usize, status: &str| {
            vec![
                change(
                    at,
                    "src/a.py",
                    &format!("try {at}"),
                    &format!("try {}", at + 1),
                ),
                llm(at + 1, json!({})),
                bash(at + 2, "pytest -q", status),
                llm(at + 3, json!({})),
            ]
        };
        let failing: Vec<_> = [0, 4, 8]
            .iter()
            .flat_map(|at| round(*at, "error"))
            .collect();
        assert_eq!(
            fires("edit_thrash", &failing),
            1,
            "three edits, each followed by a failing test"
        );
        assert_eq!(fires("edit_thrash", &failing[..8]), 0, "two rounds");
        // A passing run between breaks the streak.
        let passing: Vec<_> = [(0, "error"), (4, "ok"), (8, "error")]
            .iter()
            .flat_map(|(at, status)| round(*at, status))
            .collect();
        let mut passing = passing;
        passing.extend(round(12, "error"));
        assert_eq!(fires("edit_thrash", &passing), 0);
        // Failing tests after edits of different files are not one file
        // thrashing.
        let spread: Vec<_> = [0_usize, 4, 8]
            .iter()
            .flat_map(|at| {
                vec![
                    change(*at, &format!("src/f{at}.py"), "a", "b"),
                    bash(at + 1, "pytest -q", "error"),
                ]
            })
            .collect();
        assert_eq!(fires("edit_thrash", &renumbered(spread)), 0);
    }

    #[test]
    fn edit_thrash_weighs_less_when_the_run_succeeded() {
        let thrash = detector("edit_thrash").expect("exists");
        assert_eq!(thrash.severity, Severity::Medium);
        assert_eq!(thrash.severity_if_succeeded, Some(Severity::Low));
        // Every other detector keeps one severity whatever the outcome.
        assert!(DETECTORS
            .iter()
            .filter(|detector| detector.name != "edit_thrash")
            .all(|detector| detector.severity_if_succeeded.is_none()));
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

    /// A model call carrying `tokens` of context, naming the conversation
    /// and agent given.
    fn call(
        index: usize,
        tokens: u64,
        conversation: Option<&str>,
        agent: Option<&str>,
    ) -> crate::Span {
        let mut extra = json!({"gen_ai.usage.input_tokens": tokens});
        if let Some(conversation) = conversation {
            extra["gen_ai.conversation.id"] = json!(conversation);
        }
        if let Some(agent) = agent {
            extra["gen_ai.agent.name"] = json!(agent);
        }
        llm(index, extra)
    }

    #[test]
    fn context_blowup_compares_calls_of_one_conversation() {
        // A sub-agent's small first call, then the main thread's next one:
        // two contexts that were never one, so no blow-up.
        let (main, sub) = (Some("sess"), Some("sess/agent-1"));
        let (coder, explorer) = (Some("claude-code"), Some("Explore"));
        assert_eq!(
            fires(
                "context_blowup",
                &[call(0, 1_500, sub, explorer), call(1, 30_000, main, coder)]
            ),
            0,
            "a sub-agent's call against the main thread's"
        );
        // The main thread's own growth, with a sub-agent's call between.
        assert_eq!(
            fires(
                "context_blowup",
                &[
                    call(0, 2_000, main, coder),
                    call(1, 1_200, sub, explorer),
                    call(2, 4_100, main, coder),
                ]
            ),
            1
        );
        // A sub-agent's own growth counts too.
        assert_eq!(
            fires(
                "context_blowup",
                &[
                    call(0, 1_200, sub, explorer),
                    call(1, 40_000, main, coder),
                    call(2, 2_500, sub, explorer),
                ]
            ),
            1
        );
        // Two sub-agents of one type are two conversations.
        assert_eq!(
            fires(
                "context_blowup",
                &[
                    call(0, 1_200, sub, explorer),
                    call(1, 9_000, Some("sess/agent-2"), explorer),
                ]
            ),
            0
        );
        // No conversation named: the agent decides.
        let (planner, writer) = (Some("planner"), Some("writer"));
        assert_eq!(
            fires(
                "context_blowup",
                &[call(0, 1_200, None, planner), call(1, 9_000, None, writer)]
            ),
            0,
            "two agents' contexts"
        );
        assert_eq!(
            fires(
                "context_blowup",
                &[call(0, 2_000, None, planner), call(1, 4_100, None, planner)]
            ),
            1
        );
        // One call naming a conversation and one naming none were not one
        // context either.
        assert_eq!(
            fires(
                "context_blowup",
                &[call(0, 1_200, main, None), call(1, 9_000, None, None)]
            ),
            0
        );
        assert_eq!(
            fires(
                "context_blowup",
                &[call(0, 1_200, None, planner), call(1, 9_000, None, None)]
            ),
            0
        );
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
