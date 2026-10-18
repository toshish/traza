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

#[test]
fn long_names_are_cut_at_a_character_boundary() {
    let long = format!("tool:{}", "é".repeat(200));
    let cut = stem(&long);
    assert!(cut.len() <= MAX_NAME_BYTES + "…".len());
    assert!(cut.ends_with('…'));
    assert_eq!(stem("tool:short"), "tool:short");
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
            end: last.end_time_ns,
            trace: last.trace_id.as_str().into(),
            span: last.span_id.as_str().into(),
            error: last.status == "error",
        };
        let declared = spans
            .iter()
            .filter(|span| span.attributes.contains_key("session.outcome"))
            .max_by_key(|span| (span.end_time_ns, &span.trace_id, &span.span_id))
            .map(|span| Declared {
                end: span.end_time_ns,
                trace: span.trace_id.as_str().into(),
                span: span.span_id.as_str().into(),
                text: span.attributes["session.outcome"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            });
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
    assert_eq!(
        (healthy.support, healthy.node.as_ref().map(|n| n.success)),
        (60, Some(54))
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
    assert!(map.truncated && map.bounds.node_cap);
    // Four nodes beyond the root: the shared first step, then its three
    // children by visits — nothing deeper.
    assert_eq!(map.nodes_total, 5);
    let llm = child(&map.root, "llm:gpt-4o");
    assert_eq!(llm.children.len(), 3);
    assert!(llm.children.iter().all(|child| child.children.is_empty()));
    assert_eq!(child(llm, "tool:search_orders").cut, 90);
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
    // The branch is chosen at depth 2, and it decides most of the outcome.
    let branch = at(2);
    assert_eq!((branch.runs, branch.covered), (180, 180));
    assert!(branch.skill.is_some_and(|skill| skill > 0.2), "{branch:?}");
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
