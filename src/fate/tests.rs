//! Unit tests for the Fate Map, against the behaviour corpus whose ground
//! truth is known exactly (see [`crate::seed::behaviour_corpus`]).

use super::*;
use crate::seed::{behaviour_corpus, BehaviourOptions};
use crate::tape::tests::{span, TempStore};
use serde_json::{json, Value};

fn now() -> u64 {
    crate::mcp::unix_nanos_now()
}

fn corpus(now_ns: u64, tenant: &str, live: bool) -> Vec<Span> {
    behaviour_corpus(&BehaviourOptions {
        now_ns,
        tenant: tenant.to_owned(),
        live,
        ..BehaviourOptions::default()
    })
    .spans
}

fn seeded(label: &str, config: crate::Config, live: bool) -> (TempStore, u64) {
    let fixture = TempStore::open(label, config);
    let now = now();
    fixture
        .store()
        .ingest_batch(corpus(now, "", live))
        .expect("ingests");
    (fixture, now)
}

/// The child of `node` whose token is `token`.
fn child<'m>(node: &'m MapNode, token: &str) -> &'m MapNode {
    node.children
        .iter()
        .find(|child| child.token == token)
        .unwrap_or_else(|| {
            panic!(
                "no child {token} under {} (children: {:?})",
                node.token,
                node.children
                    .iter()
                    .map(|child| &child.token)
                    .collect::<Vec<_>>()
            )
        })
}

fn wilson_pair(successes: u64, trials: u64) -> [f64; 2] {
    let (low, high) = stats::wilson(successes, trials);
    [stats::round4(low), stats::round4(high)]
}

#[test]
fn repeats_fold_into_buckets_and_paths_stop_at_their_depth() {
    assert_eq!(
        [1, 2, 3, 5, 6, 40].map(bucket),
        ["", "×2", "×3-5", "×3-5", "×6+", "×6+"]
    );
    let grouped = groups(["a", "a", "b", "c", "c", "c", "c", "c", "c", "a"]);
    let grouped: Vec<(String, usize)> = grouped
        .into_iter()
        .map(|(name, count)| (name.to_owned(), count))
        .collect();
    assert_eq!(path_of(&grouped, true), ["a×2", "b", "c×6+", "a"]);
    // A run read only in part: its last group may be longer than it looks,
    // so it is dropped and the path ends in the fold token...
    assert_eq!(path_of(&grouped, false), ["a×2", "b", "c×6+", FOLD_TOKEN]);
    // ...unless it is already ×6+, which more of the same cannot change.
    assert_eq!(
        path_of(&grouped[..3], false),
        ["a×2", "b", "c×6+", FOLD_TOKEN]
    );
    let deep: Vec<(String, usize)> = (0..100)
        .map(|index| (format!("tool:t{}", index % 2), 1))
        .collect();
    let path = path_of(&deep, true);
    assert_eq!(path.len(), MAX_DEPTH + 1);
    assert_eq!(path.last().map(String::as_str), Some(FOLD_TOKEN));
    assert!(path_of(&[], true).is_empty());
}

/// A reading of `groups` (`(stem, count)`, names known) by agent `agent`.
fn reading(agent: &str, groups: &[(&str, usize)], complete: bool, live: bool) -> Reading {
    Reading {
        agent_key: stem_key(agent),
        agent: Some(agent.to_owned()),
        groups: groups
            .iter()
            .map(|(name, count)| ReadGroup {
                key: stem_key(name),
                count: *count,
                name: Some((*name).to_owned()),
            })
            .collect(),
        complete,
        live,
        steps: groups.iter().map(|(_, count)| *count as u64).sum(),
    }
}

#[test]
fn a_path_read_in_part_is_judged_on_what_was_read_never_called_novel() {
    let (fixture, now) = seeded("fold-token", crate::Config::default(), false);
    let store = fixture.store();
    let build = store
        .fate
        .get_or_build(store, &BuildKey::of(Some(""), None, None), now, None)
        .expect("builds");
    let cohort = build.cohort("", "support-agent").expect("cohort");
    let view = View {
        cohort,
        left_out: None,
    };
    // Read in part: the open search group is dropped, the path folds.
    let partial = reading(
        "support-agent",
        &[("llm:gpt-4o", 1), ("tool:search_orders", 1)],
        false,
        true,
    );
    assert_eq!(partial.path(), ["llm:gpt-4o", FOLD_TOKEN]);
    let probe = partial.probe();
    let walked = walk(&build, &view, &probe.closed);
    assert_eq!(walked.matched.len(), 2);
    assert!(walked.stopped.is_none());
    let judged = judge(&build, &view, &probe, DEFAULT_VERDICT_SUPPORT);
    assert_eq!(judged.verdict, ForesightVerdict::OnTrack);
    assert_eq!(judged.here.map(|here| here.counts.visits), Some(180));
    // A token no run took at that point is new, though — open or closed.
    for live in [true, false] {
        let novel = reading(
            "support-agent",
            &[("llm:gpt-4o", 1), ("tool:summon", 1)],
            true,
            live,
        );
        assert_eq!(
            judge(&build, &view, &novel.probe(), DEFAULT_VERDICT_SUPPORT).verdict,
            ForesightVerdict::Novel
        );
    }
}

#[test]
fn a_live_path_keeps_the_earliest_steps_in_order_within_its_window() {
    // The guard's budget is documented in records of this size.
    assert_eq!(std::mem::size_of::<LiveStep>(), 48);
    let mut path = LivePath::default();
    let token = |start: u64, signature: &str| StepToken::named(start, "s", signature);
    path.land(1, Some(&token(30, "tool:c")));
    path.land(2, Some(&token(10, "tool:a")));
    path.land(3, Some(&token(20, "tool:a")));
    let reading = path.reading(Some("svc"), false, 3);
    let groups: Vec<(u64, usize)> = reading
        .groups
        .iter()
        .map(|group| (group.key, group.count))
        .collect();
    assert_eq!(groups, [(stem_key("tool:a"), 2), (stem_key("tool:c"), 1)]);
    assert_eq!(reading.agent_key, stem_key("svc"));
    // A new version moves its step; one that is no step takes it out.
    path.land(1, Some(&token(5, "tool:c")));
    path.land(3, None);
    assert_eq!(path.len(), 2);
    // Full: later steps are dropped, earlier ones displace the latest,
    // and the path says it is no longer whole.
    for index in 0..RUN_STEP_WINDOW as u64 {
        path.land(100 + index, Some(&token(1_000 + index, "tool:x")));
    }
    assert_eq!(path.len(), RUN_STEP_WINDOW);
    assert!(!path.reading(None, false, 0).complete);
    path.land(9, Some(&token(1, "tool:first")));
    assert_eq!(path.len(), RUN_STEP_WINDOW);
    assert_eq!(
        path.reading(None, false, 0).groups[0].key,
        stem_key("tool:first")
    );
}

#[test]
fn long_names_are_cut_at_a_character_boundary() {
    let long = format!("tool:{}", "é".repeat(200));
    let cut = stem(&long);
    assert!(cut.len() <= MAX_NAME_BYTES + "…".len());
    assert!(cut.ends_with('…'));
    assert_eq!(stem("tool:short"), "tool:short");
}

#[test]
fn an_agent_with_a_long_name_finds_its_cohort_from_every_reader() {
    // A long CrewAI role as the service: the build, the map route, the run
    // route and the fuse must all name the cohort the same way.
    let fixture = TempStore::open("long-agent", crate::Config::default());
    let store = fixture.store();
    let now = now();
    let agent = format!("the {} agent", "very ".repeat(60));
    assert!(agent.len() > MAX_NAME_BYTES);
    let mut spans = Vec::new();
    for index in 0..40_u64 {
        spans.extend(synthetic_run(
            &format!("done-{index}"),
            &agent,
            &["llm:gpt-4o", "tool:lookup"],
            now - 3 * 3_600_000_000_000 - index * 60_000_000_000,
            index % 4 == 0,
        ));
    }
    spans.extend(synthetic_run(
        "going",
        &agent,
        &["llm:gpt-4o", "tool:lookup"],
        now - 30_000_000_000,
        false,
    ));
    store.ingest_batch(spans).expect("ingests");
    let map = store
        .fate_map("", &MapRequest::new(&agent), now)
        .expect("builds")
        .expect("the long name finds its cohort");
    assert_eq!(map.runs, 40);
    assert!(map.agent.len() <= MAX_NAME_BYTES + "…".len());
    let foresight = store
        .foresee("", "going", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(foresight.cohort_runs, 40, "{}", foresight.explanation);
    assert_eq!(foresight.verdict, ForesightVerdict::OnTrack);
    // The fuse's reading, from the map the route just built.
    let cached = store.foresee_cached("", "going", 30);
    assert_eq!(
        (cached.verdict, cached.support),
        (ForesightVerdict::OnTrack, 40),
        "{}",
        cached.explanation
    );
}

#[test]
fn the_outcome_read_from_two_spans_is_session_outcomes() {
    // The fold keeps only the last span to end and the last declaration;
    // session_outcome over the whole run must agree with it.
    let now = 10_000_000_000_000;
    let idle = crate::mcp::SESSION_IDLE_NS;
    let cases: Vec<Vec<(u64, &str, Option<&str>)>> = vec![
        vec![(100, "ok", None), (200, "error", None)],
        vec![(100, "error", None), (300, "ok", None)],
        vec![(100, "ok", Some("abandoned")), (300, "error", None)],
        vec![(400, "ok", Some("failed")), (300, "ok", Some("success"))],
        vec![(now - idle / 2, "ok", None)],
        vec![(now - idle / 2, "ok", Some("resolved"))],
    ];
    for (index, case) in cases.iter().enumerate() {
        let spans: Vec<Span> = case
            .iter()
            .enumerate()
            .map(|(position, (end, status, declared))| {
                let mut attributes = json!({});
                if let Some(text) = declared {
                    attributes["session.outcome"] = json!(text);
                }
                let mut span = span("t", &format!("s{position}"), end - 5, attributes);
                span.end_time_ns = *end;
                span.status = (*status).to_owned();
                span
            })
            .collect();
        let expected = attribution::session_outcome(&spans, now, idle).outcome;
        let last = spans
            .iter()
            .max_by_key(|span| (span.end_time_ns, &span.trace_id, &span.span_id))
            .expect("a span");
        let tail = Tail {
            end: end_key(last),
            error: last.status == "error",
        };
        let declared = spans
            .iter()
            .filter(|span| span.attributes.contains_key("session.outcome"))
            .max_by_key(|span| (span.end_time_ns, &span.trace_id, &span.span_id))
            .map(|span| Declared {
                end: end_key(span),
                text: span.attributes["session.outcome"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            });
        // The fold's fixed-size key picks the same spans the full ids do,
        // and a run's record of them holds no id however long.
        assert_eq!(std::mem::size_of::<Tail>(), 40);
        assert_eq!(
            spans.iter().map(end_key).max(),
            Some(end_key(last)),
            "case {index}"
        );
        assert_eq!(
            outcome_of(&tail, declared.as_ref(), now),
            expected,
            "case {index}"
        );
    }
}

#[test]
fn the_map_learns_every_branch_with_exact_counts() {
    let (fixture, now) = seeded("map", crate::Config::default(), true);
    let store = fixture.store();
    let map = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("the cohort exists");
    // 60 + 50 + 40 + 30 completed runs; the six live runs are unknown.
    assert_eq!(
        (
            map.runs,
            map.success,
            map.failure,
            map.abandoned,
            map.unknown
        ),
        (180, 98, 73, 9, 6)
    );
    assert_eq!(map.base_rate, Some(stats::round4(98.0 / 180.0)));
    assert!(!map.truncated, "{:?}", map.bounds);
    let llm = child(&map.root, "llm:gpt-4o");
    assert_eq!(llm.visits, 180);
    // The hotspot: three ways on, 90 / 50 / 40.
    assert!(llm.hotspot);
    assert!((llm.entropy - 1.4955).abs() < 1e-3, "{}", llm.entropy);
    let search = child(llm, "tool:search_orders");
    let looped = child(llm, "tool:search_orders×3-5");
    let escalate = child(llm, "tool:lookup_customer");
    assert_eq!((search.visits, search.success), (90, 69));
    assert_eq!(
        (looped.visits, looped.success, looped.abandoned),
        (50, 5, 5)
    );
    assert_eq!((escalate.visits, escalate.success), (40, 24));
    // The fatal fork: 10% against 93 of 130 for its siblings.
    assert!(looped.fatal_fork);
    let fork = looped.fork.as_ref().expect("compared");
    assert_eq!(fork.siblings_visits, 130);
    assert!((fork.gap - (93.0 / 130.0 - 0.1)).abs() < 1e-3);
    assert!(fork.p_value < 1e-9, "{}", fork.p_value);
    assert_eq!(looped.ci95, wilson_pair(5, 50));
    assert_eq!(looped.success_rate, Some(0.1));
    assert!(!search.fatal_fork && !escalate.fatal_fork);
    // Deeper: a refund issued twice is a fork of its own.
    let refunds = child(child(search, "llm:gpt-4o"), "tool:issue_refund×2");
    assert_eq!((refunds.visits, refunds.success), (30, 15));
    assert!(refunds.fatal_fork);
    // The summary lists both, most significant first.
    let summarized: Vec<&str> = map
        .fatal_forks
        .iter()
        .filter_map(|fork| fork.path.last().map(String::as_str))
        .collect();
    assert_eq!(
        summarized,
        ["tool:search_orders×3-5", "tool:issue_refund×2"],
        "{:?}",
        map.fatal_forks
    );
    assert_eq!(
        map.hotspots.first().map(|hotspot| hotspot.path.clone()),
        Some(vec!["llm:gpt-4o".to_owned()])
    );
    // Example runs: the newest of each outcome through the node.
    let outcomes: Vec<Outcome> = looped
        .example_runs
        .iter()
        .map(|example| example.outcome)
        .collect();
    assert_eq!(
        outcomes,
        [Outcome::Success, Outcome::Failure, Outcome::Abandoned]
    );
    assert!(looped
        .example_runs
        .iter()
        .all(|example| example.run.starts_with("search_loop-")));
    // Ended runs end at the final llm call of their branch.
    let ends = child(child(search, "llm:gpt-4o"), "tool:issue_refund");
    assert_eq!(child(ends, "llm:gpt-4o").ended, 60);
}

#[test]
fn a_small_bad_branch_is_not_a_fatal_fork_without_significance() {
    let (fixture, now) = seeded("significance", crate::Config::default(), false);
    let map = fixture
        .store()
        .fate_map("", &MapRequest::new("researcher"), now)
        .expect("builds")
        .expect("the researcher cohort");
    // The agent is the root step's gen_ai.agent.name, not the service.
    assert_eq!(map.agent, "researcher");
    let llm = child(
        child(&map.root, "agent:researcher"),
        "llm:claude-sonnet-4-5",
    );
    assert!(llm.hotspot);
    assert!((llm.entropy - 2.0).abs() < 1e-9);
    // 1 of 10 against 19 of 30: 53 points, and significant.
    let human = child(llm, "tool:ask_human");
    assert!(human.fatal_fork, "{:?}", human.fork);
    // 3 of 10 against 17 of 30: 27 points — past the gap rule, but ten runs
    // cannot tell that from luck, so it is not flagged.
    let db = child(llm, "tool:query_db");
    let fork = db.fork.as_ref().expect("compared");
    assert!(fork.gap >= FORK_GAP, "{fork:?}");
    assert!(fork.p_value >= FORK_ALPHA, "{fork:?}");
    assert!(!db.fatal_fork);
    assert!(fixture
        .store()
        .fate_map("", &MapRequest::new("research-svc"), now)
        .expect("builds")
        .is_none());
}

#[test]
fn cohorts_list_agents_their_tasks_and_what_was_excluded() {
    let (fixture, now) = seeded("cohorts", crate::Config::default(), true);
    let listing = fixture
        .store()
        .fate_cohorts(Some(""), None, now)
        .expect("lists");
    let support = &listing.cohorts[0];
    assert_eq!(
        (
            support.agent.as_str(),
            support.runs,
            support.success,
            support.failure,
            support.abandoned,
            support.unknown
        ),
        ("support-agent", 180, 98, 73, 9, 6)
    );
    let task = &listing.cohorts[1];
    let key = crate::payload::sha256_hex(b"escalate billing dispute")[..16].to_owned();
    assert_eq!(task.task.as_deref(), Some(key.as_str()));
    assert_eq!(
        (task.runs, task.success, task.failure, task.abandoned),
        (40, 24, 12, 4)
    );
    let researcher = listing
        .cohorts
        .iter()
        .find(|cohort| cohort.agent == "researcher")
        .expect("listed");
    assert_eq!((researcher.runs, researcher.success), (40, 20));
    // Narrowed to the task, the map is just those runs.
    let mut request = MapRequest::new("support-agent");
    request.task = Some(key);
    let narrowed = fixture
        .store()
        .fate_map("", &request, now)
        .expect("builds")
        .expect("exists");
    assert_eq!((narrowed.runs, narrowed.success), (40, 24));
    assert_eq!(narrowed.unknown, 0, "live runs declare no task");
}

#[test]
fn foresight_gives_every_verdict_its_case() {
    let (fixture, now) = seeded("foresight", crate::Config::default(), true);
    let store = fixture.store();
    let foresee = |run: &str| {
        store
            .foresee("", run, DEFAULT_VERDICT_SUPPORT, now)
            .expect("reads")
            .expect("the run exists")
    };
    for run in ["live-doomed-0", "live-doomed-1", "live-doomed-2"] {
        let doomed = foresee(run);
        assert_eq!(doomed.verdict, ForesightVerdict::LikelyFailure, "{run}");
        assert_eq!(doomed.path, ["llm:gpt-4o", "tool:search_orders×3-5"]);
        assert_eq!((doomed.support, doomed.matched_depth), (50, 2));
        assert_eq!(doomed.ci95, Some(wilson_pair(5, 50)));
        assert!(!doomed.excluded_self, "a live run is in no map");
        assert_eq!(doomed.outcome, Outcome::Unknown);
        assert!(
            doomed
                .explanation
                .starts_with("runs that took this path succeeded 5 of 50 times (10.0%, 95% CI"),
            "{}",
            doomed.explanation
        );
        // What did better from the step before: nothing — the run is past
        // the fork. But the doomed node itself says what runs did next.
        assert_eq!(
            doomed.next.first().map(|next| next.token.as_str()),
            Some("llm:gpt-4o")
        );
    }
    let healthy = foresee("live-healthy-0");
    assert_eq!(healthy.verdict, ForesightVerdict::OnTrack);
    // One refund so far, and it may be followed by another: the runs that
    // stopped at one (54 of 60) pooled with the runs that issued a second
    // (15 of 30).
    assert!(healthy.last_open);
    let node = healthy.node.as_ref().expect("a node");
    assert_eq!((healthy.support, node.success), (90, 69));
    assert_eq!(node.token, "tool:issue_refund");
    assert_eq!(node.pooled, ["tool:issue_refund", "tool:issue_refund×2"]);
    // What they did next: the 60 that stopped refunding went back to the
    // model; the 30 that refunded again are the same step repeated.
    let next: Vec<(&str, u64, bool)> = healthy
        .next
        .iter()
        .map(|option| (option.token.as_str(), option.visits, option.repeat))
        .collect();
    assert_eq!(
        next,
        [("llm:gpt-4o", 60, false), ("tool:issue_refund", 30, true)]
    );
    let novel = foresee("live-novel-0");
    assert_eq!(novel.verdict, ForesightVerdict::Novel);
    let divergence = novel.divergence.as_ref().expect("diverged");
    assert_eq!(divergence.depth, 1);
    assert_eq!(divergence.took, "tool:delete_database");
    assert_eq!(
        divergence.common_next,
        [
            "tool:search_orders",
            "tool:search_orders×3-5",
            "tool:lookup_customer"
        ]
    );
    // From the first step, the step that did best next is recommended.
    assert_eq!(
        novel.recommended_next.as_deref(),
        Some("tool:search_orders")
    );
    let early = foresee("live-early-0");
    assert_eq!(early.verdict, ForesightVerdict::OnTrack);
    assert_eq!((early.support, early.matched_depth), (180, 1));
    assert_eq!(
        early.recommended_next.as_deref(),
        Some("tool:search_orders")
    );
    // Too little support for a verdict is said, not guessed.
    let thin = store
        .foresee("", "live-doomed-0", 51, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(thin.verdict, ForesightVerdict::InsufficientHistory);
    assert!(
        thin.explanation.contains("only 50 earlier runs"),
        "{}",
        thin.explanation
    );
    assert!(store
        .foresee("", "no-such-run", 30, now)
        .expect("reads")
        .is_none());
    assert!(matches!(
        store.foresee("", "", 30, now),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_completed_run_is_left_out_of_its_own_odds() {
    let (fixture, now) = seeded("leave-one-out", crate::Config::default(), false);
    let store = fixture.store();
    // search_loop-0 failed (the Bresenham spread puts successes later).
    let own = store
        .foresee("", "search_loop-0", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(own.outcome, Outcome::Failure);
    assert!(own.excluded_self);
    assert_eq!(own.support, 49);
    assert_eq!(own.node.as_ref().map(|node| node.failure), Some(39));
    assert_eq!(own.cohort_runs, 179);
    // A run alone on its branch is novel once it is left out.
    let lonely = vec![
        {
            let mut span = span(
                "lonely",
                "a",
                now - 3 * 3_600_000_000_000,
                json!({"session.id": "lonely", "gen_ai.operation.name": "chat",
                       "gen_ai.request.model": "gpt-4o"}),
            );
            span.service = "support-agent".into();
            span
        },
        {
            let mut span = span(
                "lonely",
                "b",
                now - 3 * 3_600_000_000_000 + 100,
                json!({"session.id": "lonely", "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": "summon_dragon"}),
            );
            span.service = "support-agent".into();
            span
        },
    ];
    store.ingest_batch(lonely).expect("ingests");
    // The cached build predates the lonely run; drop it.
    store.forget_fate();
    let alone = store
        .foresee("", "lonely", 30, now)
        .expect("reads")
        .expect("exists");
    assert!(alone.excluded_self);
    assert_eq!(
        alone.verdict,
        ForesightVerdict::Novel,
        "{}",
        alone.explanation
    );
}

#[test]
fn the_node_bound_keeps_the_shallowest_busiest_branches_and_says_so() {
    let config = crate::Config {
        fate: FateConfig {
            max_nodes: 4,
            ..FateConfig::default()
        },
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("node-cap", config, true);
    let store = fixture.store();
    let map = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    assert!(map.truncated && map.bounds.node_cap && map.nodes_capped);
    // Four nodes across the build, level by level: both agents' first
    // steps, then the two busiest second steps (support-agent's 90 and 50;
    // its 40 escalations and the researcher's 40 are cut) — nothing deeper.
    assert_eq!(map.nodes_total, 4);
    let llm = child(&map.root, "llm:gpt-4o");
    assert_eq!(llm.children.len(), 2);
    assert_eq!(llm.cut, 40);
    assert!(llm.children.iter().all(|child| child.children.is_empty()));
    assert_eq!(child(llm, "tool:search_orders").cut, 90);
    let research = store
        .fate_map("", &MapRequest::new("researcher"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!(research.nodes_total, 2);
    assert!(research.nodes_capped);
    // A path into the cut is not called novel: the map cannot say.
    let healthy = store
        .foresee("", "live-healthy-0", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(healthy.verdict, ForesightVerdict::InsufficientHistory);
    assert!(
        healthy.explanation.contains("--fate-max-nodes"),
        "{}",
        healthy.explanation
    );
    // The doomed path is shallow enough to be judged still.
    let doomed = store
        .foresee("", "live-doomed-0", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(doomed.verdict, ForesightVerdict::LikelyFailure);
}

#[test]
fn the_node_bound_is_shared_shallowest_first_so_no_cohort_is_starved() {
    // support-agent's whole tree is 13 nodes and the researcher's 10. With
    // a budget of 13, spent cohort by cohort the larger would take it all
    // and leave the researcher a bare root; spent level by level across
    // cohorts, each keeps its first levels.
    let config = crate::Config {
        fate: FateConfig {
            max_nodes: 13,
            ..FateConfig::default()
        },
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("node-share", config, false);
    let store = fixture.store();
    let research = store
        .fate_map("", &MapRequest::new("researcher"), now)
        .expect("builds")
        .expect("exists");
    assert!(research.nodes_total > 1, "{:?}", research.bounds);
    let sonnet = child(
        child(&research.root, "agent:researcher"),
        "llm:claude-sonnet-4-5",
    );
    for tool in [
        "tool:web_search",
        "tool:read_url",
        "tool:query_db",
        "tool:ask_human",
    ] {
        assert_eq!(child(sonnet, tool).visits, 10);
    }
    let support = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    // Three levels each: 1 + 3 + 3 for support-agent, 1 + 1 + 4 for the
    // researcher — thirteen.
    assert_eq!(support.nodes_total + research.nodes_total, 13 + 2);
    assert!(support.bounds.node_cap);
    // Each says the bound cut its own tree, and the researcher's still
    // reads its runs: web_search's path is matched for three tokens and
    // only the fourth is hidden by the bound, never called new.
    assert!(support.nodes_capped && research.nodes_capped);
    let web = store
        .foresee("", "web_search-0", 3, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(web.matched_depth, 3);
    assert_eq!(web.verdict, ForesightVerdict::InsufficientHistory);
    assert!(
        web.explanation.contains("--fate-max-nodes"),
        "{}",
        web.explanation
    );
}

#[test]
fn an_open_step_whose_longer_repeats_were_cut_is_not_pooled_short() {
    // Fourteen nodes: everything to depth 3 (thirteen), then the busiest
    // depth-4 child — one refund, 60 runs. The 30 runs that refunded twice
    // went down a branch the bound kept from being built.
    let config = crate::Config {
        fate: FateConfig {
            max_nodes: 14,
            ..FateConfig::default()
        },
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("cut-pool", config, true);
    let store = fixture.store();
    let map = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    let refunds = child(
        child(child(&map.root, "llm:gpt-4o"), "tool:search_orders"),
        "llm:gpt-4o",
    );
    assert_eq!(refunds.children.len(), 1);
    assert_eq!(refunds.cut, 30);
    // A run that has refunded once may refund again; pooling only the one
    // branch that was built would claim 54 of 60. The map cannot say.
    let healthy = store
        .foresee("", "live-healthy-0", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(
        healthy.verdict,
        ForesightVerdict::InsufficientHistory,
        "{}",
        healthy.explanation
    );
    assert!(healthy.explanation.contains("--fate-max-nodes"));
    // A step the bound cut nothing of is still judged.
    let doomed = store
        .foresee("", "live-doomed-0", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(doomed.verdict, ForesightVerdict::LikelyFailure);
}

#[test]
fn the_run_bound_keeps_each_cohorts_newest_runs() {
    let config = crate::Config {
        fate: FateConfig {
            max_runs: 20,
            ..FateConfig::default()
        },
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("run-cap", config, false);
    let map = fixture
        .store()
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!(map.runs, 20);
    assert!(map.truncated);
    assert!(map.bounds.runs_dropped > 0);
    // 220 runs is more than the fold tracks (four times 20), so the window
    // narrowed to its newest part rather than keeping whichever runs the
    // fold met first.
    assert!(map.bounds.window_narrowed, "{:?}", map.bounds);
    assert!(map.window.since_ns > now - DEFAULT_WINDOW_NS);
    assert!(map.bounds.runs_tracked <= 80);
    // The newest runs are the refund_happy ones (generated newest first),
    // and the map holds exactly the twenty newest.
    let refunds = child(&map.root, "llm:gpt-4o");
    assert_eq!(refunds.children.len(), 1);
    let examples: Vec<&str> = child(refunds, "tool:search_orders")
        .example_runs
        .iter()
        .map(|example| example.run.as_str())
        .collect();
    assert_eq!(examples, ["refund_happy-1", "refund_happy-0"]);
}

#[test]
fn tenants_never_see_each_other() {
    let fixture = TempStore::open("tenancy", crate::Config::default());
    let store = fixture.store();
    let now = now();
    store.ingest_batch(corpus(now, "", true)).expect("default");
    let mut acme = behaviour_corpus(&BehaviourOptions {
        now_ns: now,
        tenant: "acme".to_owned(),
        prefix: "acme-".to_owned(),
        live: false,
        ..BehaviourOptions::default()
    })
    .spans;
    // acme's support agent only ever loops.
    acme.retain(|span| {
        span.attributes
            .get("session.id")
            .and_then(Value::as_str)
            .is_some_and(|run| run.starts_with("acme-search_loop"))
    });
    store.ingest_batch(acme).expect("acme");
    let map = store
        .fate_map("acme", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!((map.runs, map.success, map.unknown), (50, 5, 0));
    assert_eq!(map.tenant, "acme");
    let ours = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!(ours.runs, 180);
    // The operator view lists both, apart.
    let every = store.fate_cohorts(None, None, now).expect("lists");
    let support: Vec<(&str, u64)> = every
        .cohorts
        .iter()
        .filter(|cohort| cohort.agent == "support-agent" && cohort.task.is_none())
        .map(|cohort| (cohort.tenant.as_str(), cohort.runs))
        .collect();
    assert_eq!(support, [("", 180), ("acme", 50)]);
    let only_acme = store.fate_cohorts(Some("acme"), None, now).expect("lists");
    assert!(only_acme
        .cohorts
        .iter()
        .all(|cohort| cohort.tenant == "acme"));
    // A run is foreseen within its own tenant only.
    assert!(store
        .foresee("acme", "live-doomed-0", 30, now)
        .expect("reads")
        .is_none());
    let theirs = store
        .foresee("acme", "acme-search_loop-1", 30, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(theirs.cohort_runs, 49);
}

#[test]
fn an_erasure_drops_every_cached_map() {
    let (fixture, now) = seeded("erasure", crate::Config::default(), false);
    let store = fixture.store();
    let before = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    let newest = before.root.example_runs[1].run.clone();
    assert_eq!(newest, "refund_happy-0", "the newest failure");
    store
        .erase(crate::erasure::Subject::Session {
            session_id: newest.clone(),
            tenant: String::new(),
        })
        .expect("erases");
    let after = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!(after.runs, 179);
    assert!(after
        .root
        .example_runs
        .iter()
        .all(|example| example.run != newest));
}

#[test]
fn a_build_honours_the_query_deadline() {
    let config = crate::Config {
        query_deadline: Some(Duration::ZERO),
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("deadline", config, false);
    let store = fixture.store();
    assert!(matches!(
        store.fate_map("", &MapRequest::new("support-agent"), now),
        Err(Error::DeadlineExceeded(_))
    ));
    assert!(matches!(
        store.fate_cohorts(None, None, now),
        Err(Error::DeadlineExceeded(_))
    ));
}

#[test]
fn requests_are_validated() {
    let fixture = TempStore::open("validate", crate::Config::default());
    let store = fixture.store();
    let now = now();
    for (request, needle) in [
        (MapRequest::new(""), "agent is required"),
        (
            MapRequest {
                depth: 0,
                ..MapRequest::new("a")
            },
            "depth must be",
        ),
        (
            MapRequest {
                depth: 66,
                ..MapRequest::new("a")
            },
            "depth must be",
        ),
        (
            MapRequest {
                min_support: 0,
                ..MapRequest::new("a")
            },
            "min_support",
        ),
        (
            MapRequest {
                max_nodes: MAX_MAP_NODES + 1,
                ..MapRequest::new("a")
            },
            "max_nodes",
        ),
        (
            MapRequest {
                window: Some((10, 5)),
                ..MapRequest::new("a")
            },
            "since must not be after until",
        ),
    ] {
        match store.fate_map("", &request, now) {
            Err(Error::Invalid(message)) => assert!(message.contains(needle), "{message}"),
            other => panic!("{request:?}: expected {needle}, got {other:?}"),
        }
    }
}

#[test]
fn the_cache_serves_until_its_lifetime_and_never_with_zero() {
    let (fixture, now) = seeded("cache", crate::Config::default(), false);
    let store = fixture.store();
    let first = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    // More runs arrive; the cached build still answers.
    store
        .ingest_batch(
            behaviour_corpus(&BehaviourOptions {
                now_ns: now,
                prefix: "more-".to_owned(),
                ..BehaviourOptions::default()
            })
            .spans,
        )
        .expect("ingests");
    let cached = store
        .fate_map("", &MapRequest::new("support-agent"), now + 1)
        .expect("serves")
        .expect("exists");
    assert_eq!(cached.runs, first.runs);
    assert_eq!(cached.built_at_ns, first.built_at_ns);
    // A zero lifetime builds every time.
    let config = crate::Config {
        fate: FateConfig {
            cache_ttl: Duration::ZERO,
            ..FateConfig::default()
        },
        ..crate::Config::default()
    };
    let (uncached, now) = seeded("no-cache", config, false);
    let a = uncached
        .store()
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    let b = uncached
        .store()
        .fate_map("", &MapRequest::new("support-agent"), now + 7)
        .expect("builds")
        .expect("exists");
    assert_eq!(b.built_at_ns, a.built_at_ns + 7);
    // And a zero lifetime gives the fuses nothing to read.
    assert!(uncached
        .store()
        .fate
        .peek(&BuildKey::of(Some(""), None, None))
        .is_none());
}

#[test]
fn a_map_reports_how_good_its_foresight_has_been() {
    let (fixture, now) = seeded("calibration", crate::Config::default(), false);
    let map = fixture
        .store()
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    let at = |depth: usize| {
        map.calibration
            .depths
            .iter()
            .find(|point| point.depth == depth)
            .unwrap_or_else(|| panic!("depth {depth}: {:?}", map.calibration))
    };
    // Every run's first step is the same, so depth 1 knows nothing the base
    // rate does not.
    assert!(
        at(1).skill.is_some_and(|skill| skill.abs() < 0.02),
        "{:?}",
        at(1)
    );
    // The branch is chosen at depth 2, and it decides most of the outcome —
    // but only partly at the moment it is chosen: a run's first search
    // cannot tell whether it will loop (the 50 that did are pooled with the
    // 90 that searched once), while its fourth can. By depth 3 the search
    // group is closed and the map knows which branch the run is on.
    let branch = at(2);
    assert_eq!((branch.runs, branch.covered), (180, 180));
    let skill = |depth: usize| at(depth).skill.unwrap_or(f64::NAN);
    assert!((0.15..0.3).contains(&skill(2)), "{branch:?}");
    assert!(skill(3) > skill(2), "{:?}", map.calibration);
    assert!(branch.brier < branch.baseline_brier);
    assert!(!map.calibration.reliability.is_empty());
    let total: u64 = map.calibration.reliability.iter().map(|bin| bin.runs).sum();
    assert_eq!(total, at(3).runs);
}

#[test]
fn a_build_is_deterministic() {
    let (one, now) = seeded("determinism-a", crate::Config::default(), true);
    let two = TempStore::open("determinism-b", crate::Config::default());
    two.store()
        .ingest_batch(corpus(now, "", true))
        .expect("ingests");
    let render = |store: &Store| {
        let map = store
            .fate_map("", &MapRequest::new("support-agent"), now)
            .expect("builds")
            .expect("exists");
        serde_json::to_string(&map).expect("serializes")
    };
    assert_eq!(render(one.store()), render(two.store()));
}

#[test]
fn rendering_prunes_by_support_depth_and_node_count_and_says_so() {
    let (fixture, now) = seeded("render", crate::Config::default(), false);
    let store = fixture.store();
    let shallow = store
        .fate_map(
            "",
            &MapRequest {
                depth: 1,
                ..MapRequest::new("support-agent")
            },
            now,
        )
        .expect("builds")
        .expect("exists");
    let llm = child(&shallow.root, "llm:gpt-4o");
    assert!(llm.children.is_empty());
    assert_eq!((llm.omitted_children, llm.omitted_visits), (3, 180));
    let picky = store
        .fate_map(
            "",
            &MapRequest {
                min_support: 45,
                ..MapRequest::new("support-agent")
            },
            now,
        )
        .expect("builds")
        .expect("exists");
    let tokens: Vec<&str> = child(&picky.root, "llm:gpt-4o")
        .children
        .iter()
        .map(|child| child.token.as_str())
        .collect();
    assert_eq!(tokens, ["tool:search_orders", "tool:search_orders×3-5"]);
    let few = store
        .fate_map(
            "",
            &MapRequest {
                max_nodes: 2,
                ..MapRequest::new("support-agent")
            },
            now,
        )
        .expect("builds")
        .expect("exists");
    assert_eq!(few.nodes_shown, 3);
    assert!(few.nodes_total > few.nodes_shown);
}

#[test]
fn predictive_limits_act_on_checks_from_a_cached_map_only() {
    let (fixture, _) = seeded("guard", crate::Config::default(), true);
    let store = fixture.store();
    store
        .put_fuse_policy(crate::guard::PolicySpec {
            name: "support".into(),
            matches: crate::guard::PolicyMatch {
                service: Some("support-agent".into()),
            },
            limits: crate::guard::Limits {
                min_success_odds: Some(0.7),
                ..crate::guard::Limits::default()
            },
            action: Action::Stop,
            ..crate::guard::PolicySpec::default()
        })
        .expect("policy");
    let check = |run: &str, tool: Option<&str>| {
        store
            .fuse_check(
                "",
                run,
                &NextStep {
                    tool: tool.map(str::to_owned),
                    ..NextStep::default()
                },
            )
            .expect("verdict")
    };
    // Cold: nothing cached, nothing built on the request path.
    let cold = check("live-doomed-0", None);
    assert_eq!(cold.verdict, Verdict::Continue);
    let foresight = cold.foresight.as_ref().expect("evaluated");
    assert_eq!(foresight.verdict, ForesightVerdict::InsufficientHistory);
    assert!(!foresight.raised && foresight.map_built_at_ns.is_none());
    // The warmer builds what the check asked for.
    assert_eq!(store.maintain_fate().expect("warms"), 1);
    assert_eq!(store.maintain_fate().expect("nothing left"), 0);
    let doomed = check("live-doomed-0", None);
    assert_eq!(doomed.verdict, Verdict::Stop);
    assert_eq!(
        doomed.reasons.last().map(String::as_str),
        Some("foresight: runs that took this path succeeded 5 of 50 times (≤21.4%)")
    );
    assert!(
        doomed.tripped_by.is_none(),
        "a prediction is not a latched trip"
    );
    // The same question as a plain verdict query.
    assert_eq!(
        store
            .fuse_verdict("", "live-doomed-0")
            .expect("verdict")
            .verdict,
        Verdict::Stop
    );
    // A healthy path continues...
    let healthy = check("live-healthy-0", None);
    assert_eq!(healthy.verdict, Verdict::Continue);
    assert!(healthy.foresight.as_ref().is_some_and(|f| !f.raised));
    // ...until the check names the step that would take it somewhere bad:
    // runs that issued the refund twice succeeded 15 of 30 times.
    let second_refund = check("live-healthy-0", Some("issue_refund"));
    assert_eq!(second_refund.verdict, Verdict::Stop);
    let foresight = second_refund.foresight.as_ref().expect("evaluated");
    assert_eq!(foresight.next_tool.as_deref(), Some("issue_refund"));
    assert_eq!((foresight.support, foresight.successes), (30, 15));
    assert!(second_refund
        .reasons
        .last()
        .is_some_and(|reason| reason.contains("this path and then called tool:issue_refund")));
    // The interval's upper bound is what is compared, not the estimate: at
    // odds of 0.55, 15 of 30 (50%, upper bound 66.8%) is not confident
    // enough to stop anything.
    let warn_policy = |odds: f64| {
        store
            .put_fuse_policy(crate::guard::PolicySpec {
                name: "support".into(),
                matches: crate::guard::PolicyMatch {
                    service: Some("support-agent".into()),
                },
                limits: crate::guard::Limits {
                    min_success_odds: Some(odds),
                    ..crate::guard::Limits::default()
                },
                action: Action::Warn,
                ..crate::guard::PolicySpec::default()
            })
            .expect("policy")
    };
    warn_policy(0.55);
    let hesitant = check("live-healthy-0", Some("issue_refund"));
    assert_eq!(hesitant.verdict, Verdict::Continue);
    assert!(hesitant.foresight.as_ref().is_some_and(|f| !f.raised));
    // And a warn policy warns rather than stops.
    warn_policy(0.7);
    let warned = check("live-healthy-0", Some("issue_refund"));
    assert_eq!(warned.verdict, Verdict::Warn);
    assert!(warned.foresight.as_ref().is_some_and(|f| f.raised));
    store
        .put_fuse_policy(crate::guard::PolicySpec {
            name: "support".into(),
            matches: crate::guard::PolicyMatch {
                service: Some("support-agent".into()),
            },
            limits: crate::guard::Limits {
                min_success_odds: Some(0.7),
                ..crate::guard::Limits::default()
            },
            action: Action::Stop,
            ..crate::guard::PolicySpec::default()
        })
        .expect("policy");
    // One step recorded is fewer than the default three: no foresight stop
    // however poor the odds (98 of 180 is below 0.7).
    let early = check("live-early-0", None);
    assert_eq!(early.verdict, Verdict::Continue);
    assert!(early
        .foresight
        .as_ref()
        .is_some_and(|f| !f.raised && f.support == 180));
    // Ingest never evaluates foresight: the doomed run's next span is
    // acknowledged without a foresight stop.
    let admission = store
        .ingest_batch(vec![{
            let mut span = span(
                "late",
                "x",
                now() - 1_000,
                json!({"session.id": "live-doomed-0", "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": "search_orders"}),
            );
            span.service = "support-agent".into();
            span
        }])
        .expect("ingests");
    assert!(admission.signals.iter().all(|signal| signal
        .reasons
        .iter()
        .all(|reason| !reason.starts_with("foresight"))));
    // A grant lowering the odds lets the run go on.
    let granted = store
        .fuse_reset(
            "",
            "live-doomed-0",
            Some(crate::guard::Limits {
                min_success_odds: Some(0.05),
                ..crate::guard::Limits::default()
            }),
        )
        .expect("resets");
    assert_eq!(granted.verdict, Verdict::Continue, "{:?}", granted.reasons);
    let after_grant = check("live-doomed-0", None);
    assert_eq!(after_grant.verdict, Verdict::Continue);
    assert!(after_grant.foresight.as_ref().is_some_and(|f| !f.raised));
    // A policy without the limit never reads foresight at all.
    let plain = store.fuse_verdict("", "trace:nothing").expect("verdict");
    assert!(plain.foresight.is_none());
}

#[test]
fn cached_foresight_never_builds_on_the_request_path() {
    let (fixture, _) = seeded("cached-foresight", crate::Config::default(), true);
    let store = fixture.store();
    let cold = store.foresee_cached("", "live-doomed-0", 30);
    assert_eq!(cold.verdict, ForesightVerdict::InsufficientHistory);
    assert!(cold.map_built_at_ns.is_none());
    assert!(
        store.fate.lock().expect("lock").entries.is_empty(),
        "nothing was built on the request path"
    );
    assert_eq!(store.maintain_fate().expect("warms"), 1);
    let warm = store.foresee_cached("", "live-doomed-0", 30);
    assert_eq!(warm.verdict, ForesightVerdict::LikelyFailure);
    assert_eq!((warm.support, warm.successes), (50, 5));
    assert!(
        !warm.raised,
        "raising is the fuse's decision, not foresight's"
    );
}

fn predictive_policy(store: &Store, odds: f64) {
    store
        .put_fuse_policy(crate::guard::PolicySpec {
            name: "support".into(),
            matches: crate::guard::PolicyMatch {
                service: Some("support-agent".into()),
            },
            limits: crate::guard::Limits {
                min_success_odds: Some(odds),
                ..crate::guard::Limits::default()
            },
            action: Action::Stop,
            ..crate::guard::PolicySpec::default()
        })
        .expect("policy");
}

#[test]
fn a_pre_call_check_reads_the_guards_live_path_never_the_store() {
    let (fixture, now) = seeded("live-path", crate::Config::default(), true);
    let store = fixture.store();
    predictive_policy(store, 0.7);
    let check = |run: &str, tool: Option<&str>| {
        store
            .fuse_check(
                "",
                run,
                &NextStep {
                    tool: tool.map(str::to_owned),
                    ..NextStep::default()
                },
            )
            .expect("verdict")
    };
    // The first check rebuilds the run in the guard from the store, as the
    // guard always does — and with a predictive policy in force the rebuilt
    // state keeps the run's behaviour path. The warmer builds the map.
    let _ = check("live-doomed-0", None);
    store.maintain_fate().expect("warms");
    let reads = || store.fate.run_reads.load(Ordering::Relaxed);
    let before = reads();
    for _ in 0..5 {
        let doomed = check("live-doomed-0", None);
        assert_eq!(doomed.verdict, Verdict::Stop);
        assert_eq!(
            doomed.reasons.last().map(String::as_str),
            Some("foresight: runs that took this path succeeded 5 of 50 times (≤21.4%)")
        );
    }
    assert_eq!(
        reads(),
        before,
        "every check read the run back from the store"
    );
    // The path the guard keeps is the one a read of the store gives.
    for run in [
        "live-doomed-0",
        "live-healthy-0",
        "live-novel-0",
        "live-early-0",
    ] {
        let _ = check(run, None);
        let live = store.fuse_behaviour("", run).expect("followed");
        let read = store.run_spans(Some(""), run, 1_000, None).expect("reads");
        let (stored, _) = read_spans(&read.spans, read.truncated, now);
        let keys = |reading: &Reading| -> Vec<(u64, usize)> {
            reading
                .groups
                .iter()
                .map(|group| (group.key, group.count))
                .collect()
        };
        assert_eq!(keys(&live), keys(&stored), "{run}");
        assert_eq!(
            (live.agent_key, live.complete, live.steps),
            (stored.agent_key, stored.complete, stored.steps),
            "{run}"
        );
    }
    // A step that lands moves the path at once: a second refund, from
    // memory, is now the 15-of-30 branch.
    let before = reads();
    assert_eq!(check("live-healthy-0", None).verdict, Verdict::Continue);
    let mut refund = synthetic_run(
        "live-healthy-0",
        "support-agent",
        &["tool:issue_refund"],
        now - 1_000_000_000,
        false,
    );
    refund[0].span_id = "second-refund".into();
    store.ingest_batch(refund).expect("ingests");
    let second = check("live-healthy-0", None);
    assert_eq!(second.verdict, Verdict::Stop, "{:?}", second.foresight);
    let foresight = second.foresight.as_ref().expect("foresight");
    assert_eq!((foresight.support, foresight.successes), (30, 15));
    assert_eq!(reads(), before);
}

#[test]
fn background_builds_have_their_own_budget_beyond_the_query_deadline() {
    // A query deadline so short no request-path build could finish: the
    // warmer's budget is its own, so the fuses still get their map.
    let config = crate::Config {
        query_deadline: Some(Duration::from_nanos(1)),
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("background-budget", config, true);
    let store = fixture.store();
    assert!(matches!(
        store.fate_map("", &MapRequest::new("support-agent"), now),
        Err(Error::DeadlineExceeded(_))
    ));
    let _ = store.foresee_cached("", "live-doomed-0", 30);
    assert_eq!(store.maintain_fate().expect("warms"), 1);
    let fuses = BuildKey::of(Some(""), None, None);
    assert!(store.fate.peek(&fuses).is_some());
}

#[test]
fn a_background_build_out_of_time_says_so_and_retries_over_a_shorter_window() {
    let config = crate::Config {
        fate: FateConfig {
            build_budget: Duration::ZERO,
            ..FateConfig::default()
        },
        query_deadline: Some(Duration::ZERO),
        ..crate::Config::default()
    };
    let (fixture, now) = seeded("background-failure", config, true);
    let store = fixture.store();
    let _ = store.foresee_cached("", "live-doomed-0", 30);
    assert!(matches!(
        store.maintain_fate(),
        Err(Error::DeadlineExceeded(_))
    ));
    // The fuse says why it has no map, rather than that one is coming.
    let after = store.foresee_cached("", "live-doomed-0", 30);
    assert_eq!(after.verdict, ForesightVerdict::InsufficientHistory);
    assert!(
        after.explanation.contains("ran out of time")
            && after.explanation.contains("shorter window"),
        "{}",
        after.explanation
    );
    // The metric counts it.
    let mut metrics = String::new();
    store.render_fate_metrics(&mut metrics);
    assert!(
        metrics.contains("traza_fate_builds_total{result=\"failed\"} 1\n"),
        "{metrics}"
    );
    // The next build of that key covers half the window, and says so.
    let fuses = BuildKey::of(Some(""), None, None);
    let build = store
        .fate
        .get_or_build(store, &fuses, now, None)
        .expect("builds without a deadline");
    assert_eq!(build.window.since_ns, now - DEFAULT_WINDOW_NS / 2);
    assert!(build.bounds.window_narrowed);
    assert!(metrics_after(store).contains("traza_fate_builds_total{result=\"ok\"} 1\n"));
}

fn metrics_after(store: &Store) -> String {
    let mut metrics = String::new();
    store.render_fate_metrics(&mut metrics);
    metrics
}

#[test]
fn explicit_windows_never_evict_the_maps_the_fuses_read() {
    let (fixture, now) = seeded("eviction", crate::Config::default(), false);
    let store = fixture.store();
    let default = BuildKey::of(Some(""), None, None);
    store
        .fate
        .get_or_build(store, &default, now, None)
        .expect("builds");
    // A dashboard polling with a fresh `until` each time.
    for poll in 0..(CACHE_ENTRIES as u64 + 4) {
        let window = Some((now - DEFAULT_WINDOW_NS, now + poll));
        store
            .fate_map(
                "",
                &MapRequest {
                    window,
                    ..MapRequest::new("support-agent")
                },
                now,
            )
            .expect("builds");
    }
    let state = store.fate.lock().expect("lock");
    assert!(state.entries.contains_key(&default));
}

#[test]
fn builds_beyond_the_concurrency_bound_wait_for_a_slot() {
    let (fixture, now) = seeded("slots", crate::Config::default(), false);
    let store = fixture.store();
    // Two builds of other keys are running.
    store.fate.lock().expect("lock").active = MAX_CONCURRENT_BUILDS;
    let key = BuildKey::of(Some(""), None, Some((now - DEFAULT_WINDOW_NS, now)));
    let deadline = Deadline::starting(Instant::now(), Some(Duration::from_millis(50)));
    assert!(matches!(
        store.fate.get_or_build(store, &key, now, deadline),
        Err(Error::DeadlineExceeded(_))
    ));
    {
        let state = store.fate.lock().expect("lock");
        assert!(state.entries.is_empty(), "it waited rather than built");
        assert!(!state.building.contains(&key));
    }
    store.fate.lock().expect("lock").active = 0;
    store
        .fate
        .get_or_build(store, &key, now, None)
        .expect("builds once a slot is free");
    let state = store.fate.lock().expect("lock");
    assert_eq!((state.active, state.peak), (0, 1));
}

#[test]
fn another_tenants_requests_never_evict_the_map_a_tenants_fuses_read() {
    let fixture = TempStore::open("cross-eviction", crate::Config::default());
    let store = fixture.store();
    let now = now();
    store.ingest_batch(corpus(now, "", false)).expect("default");
    store
        .ingest_batch(corpus(now, "acme", false))
        .expect("acme");
    // The default tenant's fuses have their map.
    let fuses = BuildKey::of(Some(""), None, None);
    store
        .fate
        .get_or_build(store, &fuses, now, None)
        .expect("builds");
    // A credential bound to acme asks for maps of tasks that do not exist,
    // each a cheap build of its own: none of them may push the default
    // tenant's map out.
    for task in 0..(CACHE_ENTRIES + 4) {
        let mut request = MapRequest::new("support-agent");
        request.task = Some(format!("{task:016x}"));
        store.fate_map("acme", &request, now).expect("builds");
    }
    {
        let state = store.fate.lock().expect("lock");
        assert!(
            state.entries.contains_key(&fuses),
            "a request of another tenant evicted the fuses' map"
        );
    }
    assert!(store.fate.peek(&fuses).is_some());
}

#[test]
#[ignore = "a timing probe, run by hand: cargo test --lib fate_build_timing -- --ignored --nocapture"]
fn fate_build_timing() {
    let fixture = TempStore::open("timing", crate::Config::default());
    let store = fixture.store();
    let now = now();
    let scale = std::env::var("TRAZA_FATE_SCALE")
        .ok()
        .and_then(|scale| scale.parse().ok())
        .unwrap_or(50);
    let spans = behaviour_corpus(&BehaviourOptions {
        now_ns: now,
        scale,
        ..BehaviourOptions::default()
    })
    .spans;
    let count = spans.len();
    for chunk in spans.chunks(20_000) {
        store.ingest_batch(chunk.to_vec()).expect("ingests");
    }
    store.flush().expect("flushes");
    let started = Instant::now();
    let map = store
        .fate_map("", &MapRequest::new("support-agent"), now)
        .expect("builds")
        .expect("exists");
    println!(
        "{count} spans, {} runs in the map ({} tracked), built in {:?}",
        map.runs,
        map.bounds.runs_tracked,
        started.elapsed()
    );
    let started = Instant::now();
    let _ = store
        .foresee("", "search_loop-3", 30, now)
        .expect("foresees");
    println!("foresight from the cached map in {:?}", started.elapsed());
}

/// The spans of one run of `service`: `steps` in order (`tool:name` or
/// `llm:model`), one second apart from `start`, the last one failed when
/// `failed`.
fn synthetic_run(run: &str, service: &str, steps: &[&str], start: u64, failed: bool) -> Vec<Span> {
    steps
        .iter()
        .enumerate()
        .map(|(position, step)| {
            let (kind, name) = step.split_once(':').expect("kind:name");
            let attributes = if kind == "llm" {
                json!({"session.id": run, "gen_ai.operation.name": "chat",
                       "gen_ai.request.model": name})
            } else {
                json!({"session.id": run, "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": name})
            };
            let mut span = span(
                &format!("{run}-trace"),
                &format!("{run}-{position}"),
                start + position as u64 * 1_000_000_000,
                attributes,
            );
            span.service = service.to_owned();
            if failed && position + 1 == steps.len() {
                span.status = "error".into();
            }
            span
        })
        .collect()
}

#[test]
fn a_live_runs_last_step_is_still_open_so_longer_repeats_are_pooled() {
    let (fixture, now) = seeded("open-group", crate::Config::default(), true);
    let store = fixture.store();
    // A run that has just made its first search: it cannot know yet
    // whether it will search again, so the runs it is compared with are
    // every run that searched at least once here — the 90 that searched
    // once (69 succeeded) AND the 50 that went on to loop (5 did).
    store
        .ingest_batch(synthetic_run(
            "live-searching",
            "support-agent",
            &["llm:gpt-4o", "tool:search_orders"],
            now - 20_000_000_000,
            false,
        ))
        .expect("ingests");
    let searching = store
        .foresee("", "live-searching", DEFAULT_VERDICT_SUPPORT, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(searching.outcome, Outcome::Unknown);
    assert_eq!(
        (
            searching.support,
            searching.node.as_ref().map(|node| node.success)
        ),
        (140, Some(74)),
        "{}",
        searching.explanation
    );
    // One refund issued so far: the 60 that stopped at one (54 succeeded)
    // and the 30 that issued a second (15 did).
    let healthy = store
        .foresee("", "live-healthy-0", DEFAULT_VERDICT_SUPPORT, now)
        .expect("reads")
        .expect("exists");
    assert_eq!(
        (
            healthy.support,
            healthy.node.as_ref().map(|node| node.success)
        ),
        (90, Some(69))
    );
    // A completed run is closed: it is compared with runs that stopped
    // where it stopped.
    let finished = store
        .foresee("", "refund_happy-3", DEFAULT_VERDICT_SUPPORT, now)
        .expect("reads")
        .expect("exists");
    assert_ne!(finished.outcome, Outcome::Unknown);
    assert_eq!(finished.support, 59, "its own branch, itself left out");
}

#[test]
fn calibration_scores_a_run_only_on_what_it_had_done_by_then() {
    // Two branches that part on whether the one tool is called again:
    // every run that called it once succeeded, every run that called it
    // twice failed. At the moment of its first call no run can tell which
    // it is on, so foresight at depth 1 must not look perfect.
    let fixture = TempStore::open("calibration-honest", crate::Config::default());
    let store = fixture.store();
    let now = now();
    let mut spans = Vec::new();
    for index in 0..60_u64 {
        let start = now - 3 * 3_600_000_000_000 - index * 60_000_000_000;
        spans.extend(synthetic_run(
            &format!("once-{index}"),
            "checker",
            &["tool:probe"],
            start,
            false,
        ));
        spans.extend(synthetic_run(
            &format!("twice-{index}"),
            "checker",
            &["tool:probe", "tool:probe"],
            start - 30_000_000_000,
            true,
        ));
    }
    store.ingest_batch(spans).expect("ingests");
    let map = store
        .fate_map("", &MapRequest::new("checker"), now)
        .expect("builds")
        .expect("exists");
    assert_eq!((map.runs, map.success), (120, 60));
    let depth1 = map
        .calibration
        .depths
        .iter()
        .find(|point| point.depth == 1)
        .expect("depth 1");
    // A run that stopped at one call is predicted, at that call, from the
    // runs that made at least one (half succeeded); a run that made two is
    // predicted, at its second, from the runs that made at least two (none
    // did). Brier 0.25 for half the runs and 0 for the rest, against a
    // base-rate Brier of 0.25: skill one half, not the 1.0 a look at how
    // each run's repeats ended would claim.
    let skill = depth1.skill.expect("a skill");
    assert!((0.4..0.6).contains(&skill), "{depth1:?}");
}

/// One live listing spends one query budget on the reads that give its
/// rows foresight — not a fresh budget per read — and reads nothing once
/// that budget is spent.
#[test]
fn a_live_listing_spends_one_budget_on_its_foresight_reads() {
    let fixture = TempStore::open(
        "live-budget",
        crate::Config {
            query_deadline: Some(std::time::Duration::from_nanos(1)),
            ..crate::Config::default()
        },
    );
    let store = fixture.store();
    let now = now();
    let (live, done): (Vec<Span>, Vec<Span>) =
        corpus(now, "", true).into_iter().partition(|span| {
            span.attributes
                .get("session.id")
                .and_then(Value::as_str)
                .is_some_and(|run| run.starts_with("live-"))
        });
    store.ingest_batch(done).expect("ingests");
    store.ingest_batch(live).expect("ingests");
    let _ = store.live_runs(Some(""), 6).expect("lists");
    assert_eq!(store.maintain_fate().expect("warms"), 1, "a map to read");
    let before = store.guard_rebuild_reads();
    let rows = store.live_runs(Some(""), 6).expect("lists").runs;
    let reads = store.guard_rebuild_reads() - before;
    assert!(
        reads <= 1,
        "a listing whose budget is spent starts {reads} reads, each with a budget of its own"
    );
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().all(|row| row.foresight.is_none()), "{rows:?}");
    assert!(
        rows.iter().all(|row| row.foresight_deferred),
        "every row it left unread says so: {rows:?}"
    );
}

/// A read that ran a listing out of its budget ends the listing's reads —
/// the rows it did not reach say so — and is not retried by the next
/// listing, which reads the others instead.
#[test]
fn a_live_read_that_ran_out_of_budget_is_not_retried_by_every_listing() {
    let budget = std::time::Duration::from_millis(1_000);
    let fixture = TempStore::open(
        "live-backoff",
        crate::Config {
            query_deadline: Some(budget),
            ..crate::Config::default()
        },
    );
    let store = fixture.store();
    let now = now();
    let (live, done): (Vec<Span>, Vec<Span>) =
        corpus(now, "", true).into_iter().partition(|span| {
            span.attributes
                .get("session.id")
                .and_then(Value::as_str)
                .is_some_and(|run| run.starts_with("live-"))
        });
    store.ingest_batch(done).expect("ingests");
    store.ingest_batch(live).expect("ingests");
    let _ = store.live_runs(Some(""), 6).expect("lists");
    assert_eq!(store.maintain_fate().expect("warms"), 1, "a map to read");
    // The first read outlasts the listing's whole budget.
    store.guard_on_next_rebuild(Box::new(move |_| {
        std::thread::sleep(budget + std::time::Duration::from_millis(200));
    }));
    let before = store.guard_rebuild_reads();
    let first = store.live_runs(Some(""), 6).expect("lists").runs;
    assert_eq!(
        store.guard_rebuild_reads() - before,
        1,
        "one read, then none"
    );
    assert!(
        first
            .iter()
            .all(|row| row.foresight.is_none() && row.foresight_deferred),
        "{first:?}"
    );
    let slow = first[0].run.clone();
    // The next listing skips the read that failed and makes the rest.
    let before = store.guard_rebuild_reads();
    let second = store.live_runs(Some(""), 6).expect("lists").runs;
    assert_eq!(store.guard_rebuild_reads() - before, 5, "{second:?}");
    for row in &second {
        if row.run == slow {
            assert!(row.foresight.is_none() && row.foresight_deferred, "{row:?}");
        } else {
            assert!(
                row.foresight.is_some() && !row.foresight_deferred,
                "{row:?}"
            );
        }
    }
}

#[test]
fn the_live_view_shows_foresight_from_memory_and_completes_a_bounded_few_runs() {
    let fixture = TempStore::open("live-view", crate::Config::default());
    let store = fixture.store();
    let now = now();
    // The completed history first, then the runs still in flight: the
    // live view lists the most recently changed first.
    let (live, done): (Vec<Span>, Vec<Span>) =
        corpus(now, "", true).into_iter().partition(|span| {
            span.attributes
                .get("session.id")
                .and_then(Value::as_str)
                .is_some_and(|run| run.starts_with("live-"))
        });
    store.ingest_batch(done).expect("ingests");
    store.ingest_batch(live).expect("ingests");
    let listing = || store.live_runs(Some(""), 6).expect("lists").runs;
    let foresight_of = |rows: &[crate::steer::LiveRun], run: &str| {
        rows.iter()
            .find(|row| row.run == run)
            .unwrap_or_else(|| panic!("{run} not listed"))
            .foresight
            .clone()
    };
    // No map yet: nothing to read foresight against, and nothing rebuilt —
    // but opening the view asked for the map.
    let first = listing();
    assert_eq!(first.len(), 6);
    assert!(first.iter().all(|row| row.foresight.is_none()), "{first:?}");
    assert_eq!(
        store.maintain_fate().expect("warms"),
        1,
        "the view asked for one map"
    );
    // With the map cached, the runs are read whole — at most the bound per
    // call — and their foresight comes from the guard's paths.
    let second = listing();
    assert_eq!(
        second.iter().filter(|row| row.foresight.is_some()).count(),
        LIVE_FORESIGHT_REBUILDS.min(6)
    );
    let doomed = foresight_of(&second, "live-doomed-0").expect("foresight");
    assert_eq!(doomed.verdict, ForesightVerdict::LikelyFailure);
    assert_eq!((doomed.support, doomed.p_success), (50, Some(0.1)));
    assert_eq!(doomed.ci95, Some(wilson_pair(5, 50)));
    let novel = foresight_of(&second, "live-novel-0").expect("foresight");
    assert_eq!(novel.verdict, ForesightVerdict::Novel);
    // The same answer /v1/fate/runs gives from the store.
    let stored = store
        .foresee("", "live-doomed-0", DEFAULT_VERDICT_SUPPORT, now)
        .expect("reads")
        .expect("found");
    assert_eq!(
        (stored.verdict, stored.support, stored.p_success),
        (doomed.verdict, doomed.support, doomed.p_success)
    );
    // Read whole, a run is answered from memory from then on: a step that
    // lands moves its foresight with no store read.
    let reads = store.fate.run_reads.load(Ordering::Relaxed);
    let mut refund = synthetic_run(
        "live-healthy-0",
        "support-agent",
        &["tool:issue_refund"],
        now - 1_000_000_000,
        false,
    );
    refund[0].span_id = "second-refund".into();
    store.ingest_batch(refund).expect("ingests");
    let third = listing();
    let healthy = foresight_of(&third, "live-healthy-0").expect("foresight");
    assert_eq!((healthy.support, healthy.p_success), (30, Some(0.5)));
    assert_eq!(store.fate.run_reads.load(Ordering::Relaxed), reads);

    // Past the bound, the rest wait for a later call; every call completes
    // at most the bound.
    let wide = store.live_runs(Some(""), 200).expect("lists").runs;
    let with = wide.iter().filter(|row| row.foresight.is_some()).count();
    assert!(with <= 6 + LIVE_FORESIGHT_REBUILDS, "{with}");
    let again = store.live_runs(Some(""), 200).expect("lists").runs;
    let more = again.iter().filter(|row| row.foresight.is_some()).count();
    assert_eq!(more, (with + LIVE_FORESIGHT_REBUILDS).min(again.len()));

    // With live paths off and no predictive policy, the guard keeps no path
    // to read from, and the view says nothing rather than reading the store.
    let quiet = TempStore::open(
        "live-view-off",
        crate::Config {
            fuses: crate::guard::FuseConfig {
                live_paths: false,
                ..crate::guard::FuseConfig::default()
            },
            ..crate::Config::default()
        },
    );
    quiet
        .store()
        .ingest_batch(corpus(now, "", true))
        .expect("ingests");
    let _ = quiet.store().live_runs(Some(""), 6).expect("lists");
    quiet.store().maintain_fate().expect("warms");
    let rows = quiet.store().live_runs(Some(""), 6).expect("lists").runs;
    assert!(rows.iter().all(|row| row.foresight.is_none()), "{rows:?}");
}
