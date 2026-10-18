//! The Fate Map, foresight and predictive fuses against the real binary.
//!
//! Every assertion here is against the behaviour corpus
//! ([`traza::seed::behaviour_corpus`]), whose ground truth is exact: four
//! `support-agent` branches with 54/60, 5/50, 24/40 and 15/30 successes,
//! four `researcher` branches with 9, 7, 3 and 1 in 10, and six live runs
//! still in flight. So the counts, intervals, forks and verdicts asserted
//! below are the arithmetic of that corpus, not snapshots of whatever the
//! code printed.

mod common;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{header, Server};
use serde_json::{json, Value};
use traza::seed::{behaviour_corpus, BehaviourOptions};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn corpus(tenant: &str, prefix: &str, live: bool) -> Vec<traza::Span> {
    behaviour_corpus(&BehaviourOptions {
        now_ns: now_ns(),
        tenant: tenant.to_owned(),
        prefix: prefix.to_owned(),
        live,
        ..BehaviourOptions::default()
    })
    .spans
}

fn ingest(server: &Server, spans: &[traza::Span], headers: &[(&str, &str)]) {
    let body = serde_json::to_value(spans).expect("serializes");
    let (status, reply) = server.json("POST", "/v1/spans", Some(&body), headers);
    assert_eq!(status, 200, "{reply}");
}

fn get(server: &Server, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    server.json("GET", path, None, headers)
}

fn ok(server: &Server, path: &str) -> Value {
    let (status, body) = get(server, path, &[]);
    assert_eq!(status, 200, "{path}: {body}");
    body
}

fn child<'v>(node: &'v Value, token: &str) -> &'v Value {
    node["children"]
        .as_array()
        .and_then(|children| children.iter().find(|child| child["token"] == token))
        .unwrap_or_else(|| panic!("no child {token} under {}", node["token"]))
}

/// The 95% Wilson interval, computed here independently of the server.
fn wilson(successes: f64, trials: f64) -> (f64, f64) {
    let z = 1.959_963_984_540_054_f64;
    let p = successes / trials;
    let denominator = 1.0 + z * z / trials;
    let centre = (p + z * z / (2.0 * trials)) / denominator;
    let half = z * (p * (1.0 - p) / trials + z * z / (4.0 * trials * trials)).sqrt() / denominator;
    (centre - half, centre + half)
}

fn close(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|actual| (actual - expected).abs() < 1e-4)
}

#[test]
fn the_map_learns_every_branch_and_names_the_fork_that_kills_runs() {
    let server = Server::spawn(&[]);
    ingest(&server, &corpus("", "", true), &[]);

    let listing = ok(&server, "/v1/fate/cohorts");
    let support = &listing["cohorts"][0];
    assert_eq!(support["agent"], "support-agent");
    assert_eq!(
        [
            &support["runs"],
            &support["success"],
            &support["failure"],
            &support["abandoned"],
            &support["unknown"]
        ],
        [&json!(180), &json!(98), &json!(73), &json!(9), &json!(6)]
    );
    assert!(close(&support["base_rate"], 98.0 / 180.0));
    let task = &listing["cohorts"][1];
    assert_eq!(task["agent"], "support-agent");
    assert_eq!((&task["runs"], &task["success"]), (&json!(40), &json!(24)));
    let task_key = task["task"].as_str().expect("a task key").to_owned();
    assert_eq!(task_key.len(), 16);
    assert!(listing["cohorts"]
        .as_array()
        .expect("cohorts")
        .iter()
        .any(|cohort| cohort["agent"] == "researcher" && cohort["runs"] == 40));
    assert_eq!(listing["truncated"], false);

    let map = ok(&server, "/v1/fate/map?agent=support-agent");
    assert_eq!((&map["runs"], &map["unknown"]), (&json!(180), &json!(6)));
    let llm = child(&map["root"], "llm:gpt-4o");
    assert_eq!(llm["visits"], 180);
    assert_eq!(llm["hotspot"], true, "three ways on, 90/50/40: {llm}");
    let looped = child(llm, "tool:search_orders×3-5");
    assert_eq!(
        (&looped["visits"], &looped["success"], &looped["abandoned"]),
        (&json!(50), &json!(5), &json!(5))
    );
    let (low, high) = wilson(5.0, 50.0);
    assert!(close(&looped["ci95"][0], low) && close(&looped["ci95"][1], high));
    assert_eq!(looped["fatal_fork"], true);
    assert_eq!(looped["fork"]["siblings_visits"], 130);
    assert!(looped["fork"]["p_value"].as_f64().expect("p") < 1e-9);
    assert!(close(&looped["lift"], 0.1 / (98.0 / 180.0)));
    assert_eq!(child(llm, "tool:search_orders")["fatal_fork"], false);
    // Example runs: the newest of each outcome down the doomed branch.
    let outcomes: Vec<&str> = looped["example_runs"]
        .as_array()
        .expect("examples")
        .iter()
        .map(|example| example["outcome"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(outcomes, ["success", "failure", "abandoned"]);
    assert_eq!(
        map["fatal_forks"][0]["path"],
        json!(["llm:gpt-4o", "tool:search_orders×3-5"])
    );
    assert_eq!(map["hotspots"][0]["path"], json!(["llm:gpt-4o"]));
    // The map scores itself, each run only on what it had done by the
    // moment it is scored: the branch shows partly at depth 2 (a first
    // search cannot tell a loop from a single one) and fully at depth 3.
    let skill = |depth: u64| {
        map["calibration"]["depths"]
            .as_array()
            .expect("depths")
            .iter()
            .find(|point| point["depth"] == depth)
            .and_then(|point| point["skill"].as_f64())
            .unwrap_or_else(|| panic!("depth {depth}: {}", map["calibration"]))
    };
    assert!(skill(1).abs() < 0.02, "{}", map["calibration"]);
    assert!((0.15..0.3).contains(&skill(2)), "{}", map["calibration"]);
    assert!(skill(3) > skill(2), "{}", map["calibration"]);

    // The researcher's small bad branch: 27 points worse, ten runs, not
    // significant — so not a fatal fork. Its 1-in-10 branch is.
    let research = ok(&server, "/v1/fate/map?agent=researcher");
    let sonnet = child(
        child(&research["root"], "agent:researcher"),
        "llm:claude-sonnet-4-5",
    );
    assert_eq!(child(sonnet, "tool:query_db")["fatal_fork"], false);
    assert_eq!(child(sonnet, "tool:ask_human")["fatal_fork"], true);

    // Pruning is reported, never silent.
    let picky = ok(
        &server,
        "/v1/fate/map?agent=support-agent&min_support=45&depth=2",
    );
    let llm = child(&picky["root"], "llm:gpt-4o");
    assert_eq!(
        llm["children"].as_array().map(Vec::len),
        Some(2),
        "40 runs is below min_support 45: {llm}"
    );
    assert_eq!(
        (&llm["omitted_children"], &llm["omitted_visits"]),
        (&json!(1), &json!(40))
    );
    assert!(child(llm, "tool:search_orders")["children"]
        .as_array()
        .is_some_and(Vec::is_empty));
    // Narrowed to the task.
    let narrowed = ok(
        &server,
        &format!("/v1/fate/map?agent=support-agent&task={task_key}"),
    );
    assert_eq!(
        (&narrowed["runs"], &narrowed["success"]),
        (&json!(40), &json!(24))
    );
    let (status, missing) = get(&server, "/v1/fate/map?agent=nobody", &[]);
    assert_eq!(status, 404, "{missing}");
}

#[test]
fn foresight_reads_a_live_run_against_its_agents_history() {
    let server = Server::spawn(&[]);
    ingest(&server, &corpus("", "", true), &[]);

    let doomed = ok(&server, "/v1/fate/runs/live-doomed-0");
    assert_eq!(doomed["verdict"], "likely_failure");
    assert_eq!(doomed["agent"], "support-agent");
    assert_eq!(
        doomed["path"],
        json!(["llm:gpt-4o", "tool:search_orders×3-5"])
    );
    assert_eq!(
        (&doomed["support"], &doomed["matched_depth"]),
        (&json!(50), &json!(2))
    );
    assert!(close(&doomed["p_success"], 0.1));
    assert!(close(&doomed["base_rate"], 98.0 / 180.0));
    assert_eq!(doomed["outcome"], "unknown");
    let explanation = doomed["explanation"].as_str().expect("explanation");
    assert!(
        explanation.starts_with(
            "runs that took this path succeeded 5 of 50 times (10.0%, 95% CI 4.3–21.4%)"
        ),
        "{explanation}"
    );
    assert!(
        explanation.ends_with("overall success rate is 54%"),
        "{explanation}"
    );

    // One refund issued so far, and the run is still going: it may yet
    // issue a second. It is compared with every run that issued at least
    // one here — 54 of 60 stopped at one and succeeded, 15 of 30 issued a
    // second — never with only the runs that stopped at one, which would
    // be knowing what it does next.
    let healthy = ok(&server, "/v1/fate/runs/live-healthy-0");
    assert_eq!(healthy["verdict"], "on_track");
    assert_eq!(healthy["last_open"], true);
    assert_eq!(
        (&healthy["support"], &healthy["node"]["success"]),
        (&json!(90), &json!(69))
    );
    assert_eq!(
        healthy["node"]["pooled"],
        json!(["tool:issue_refund", "tool:issue_refund×2"])
    );
    assert_eq!(
        healthy["next"][1],
        json!({"token": "tool:issue_refund", "visits": 30, "success_rate": 0.5,
               "ci95": [0.3315, 0.6685], "repeat": true})
    );

    let novel = ok(&server, "/v1/fate/runs/live-novel-0");
    assert_eq!(novel["verdict"], "novel");
    assert_eq!(
        novel["divergence"],
        json!({"depth": 1, "took": "tool:delete_database",
               "common_next": ["tool:search_orders", "tool:search_orders×3-5",
                               "tool:lookup_customer"]})
    );
    assert_eq!(novel["recommended_next"], "tool:search_orders");

    // A completed run is judged with itself left out.
    let finished = ok(&server, "/v1/fate/runs/search_loop-0");
    assert_eq!(finished["outcome"], "failure");
    assert_eq!(finished["excluded_self"], true);
    assert_eq!(
        (&finished["support"], &finished["cohort_runs"]),
        (&json!(49), &json!(179))
    );

    // Support below the bar is said, not guessed.
    let thin = ok(&server, "/v1/fate/runs/live-doomed-0?min_support=51");
    assert_eq!(thin["verdict"], "insufficient_history");
    let (status, _) = get(&server, "/v1/fate/runs/no-such-run", &[]);
    assert_eq!(status, 404);
    // A run id carrying a slash travels percent-encoded.
    let (status, _) = get(&server, "/v1/fate/runs/a%2Fb", &[]);
    assert_eq!(status, 404);
}

#[test]
fn a_predictive_fuse_stops_a_doomed_run_at_the_check_and_never_on_ingest() {
    let server = Server::spawn(&[]);
    ingest(&server, &corpus("", "", true), &[]);
    let (status, policy) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(
            &json!({"name": "support", "match": {"service": "support-agent"},
                     "limits": {"min_success_odds": 0.7}, "action": "stop"}),
        ),
        &[],
    );
    assert_eq!(status, 200, "{policy}");
    assert_eq!(policy["limits"], json!({"min_success_odds": 0.7}));
    let check = |run: &str, next: Value| {
        let (status, verdict) = server.json(
            "POST",
            "/v1/fuses/check",
            Some(&json!({"run": run, "next": next})),
            &[],
        );
        assert_eq!(status, 200, "{verdict}");
        verdict
    };
    // Cold: the check reads only a cached map, so it answers with no
    // foresight stop and asks the background warmer for one.
    let cold = check("live-doomed-0", json!({}));
    assert_eq!(cold["verdict"], "continue", "{cold}");
    assert_eq!(cold["foresight"]["verdict"], "insufficient_history");
    assert_eq!(cold["foresight"]["raised"], false);
    // The warmer runs every two seconds; give it ample time.
    let started = Instant::now();
    let doomed = loop {
        let verdict = check("live-doomed-0", json!({}));
        if verdict["foresight"]["map_built_at_ns"].is_u64() {
            break verdict;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the warmer never built the map: {verdict}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(doomed["verdict"], "stop", "{doomed}");
    assert_eq!(
        doomed["reasons"],
        json!(["foresight: runs that took this path succeeded 5 of 50 times (≤21.4%)"])
    );
    assert_eq!(doomed["foresight"]["raised"], true);
    assert_eq!(doomed["foresight"]["verdict"], "likely_failure");
    assert!(
        doomed["tripped_by"].is_null(),
        "a prediction is not a latched trip"
    );
    // The warmer's builds are counted where operators look.
    let (status, _, metrics) = server.request("GET", "/v1/metrics", None, &[]);
    assert_eq!(status, 200);
    assert!(
        metrics.contains("traza_fate_builds_total{result=\"ok\"} ")
            && metrics.contains("traza_fate_builds_total{result=\"failed\"} 0\n")
            && metrics.contains("traza_fate_cached_builds 1\n"),
        "{metrics}"
    );
    let (_, queried) = get(&server, "/v1/fuses/runs/live-doomed-0", &[]);
    assert_eq!(queried["verdict"], "stop", "verdict queries foresee too");
    // The listing is answered from memory and foresees nothing.
    let (_, listed) = get(&server, "/v1/fuses/runs?state=stop", &[]);
    assert!(
        listed["runs"]
            .as_array()
            .is_some_and(|runs| runs.iter().all(|run| run["run"] != "live-doomed-0")),
        "{listed}"
    );

    // A healthy run continues — until the check names the step that would
    // take it down a worse branch: a second refund (15 of 30 succeeded).
    assert_eq!(check("live-healthy-0", json!({}))["verdict"], "continue");
    let second = check("live-healthy-0", json!({"tool": "issue_refund"}));
    assert_eq!(second["verdict"], "stop", "{second}");
    assert_eq!(second["foresight"]["next_tool"], "issue_refund");
    assert_eq!(
        (
            &second["foresight"]["support"],
            &second["foresight"]["successes"]
        ),
        (&json!(30), &json!(15))
    );
    // One step recorded: below the default foresight_after_steps of three.
    let early = check("live-early-0", json!({}));
    assert_eq!(early["verdict"], "continue");
    assert_eq!(early["foresight"]["raised"], false);

    // Ingest never foresees: the doomed run's next step is acknowledged
    // without a fuse header.
    let late = json!([{
        "trace_id": "late-trace", "span_id": "late", "name": "execute_tool search_orders",
        "service": "support-agent", "status": "ok",
        "start_time_ns": now_ns() - 1_000_000, "end_time_ns": now_ns() - 500_000,
        "attributes": {"session.id": "live-doomed-0", "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": "search_orders"},
    }]);
    let (status, headers, body) = server.request(
        "POST",
        "/v1/spans",
        Some(serde_json::to_vec(&late).expect("encodes").as_slice()),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert!(header(&headers, "Traza-Fuse").is_none(), "{body}");
    assert!(!body.contains("foresight"), "{body}");

    // A grant that lowers the odds lets the run go on.
    let (status, reset) = server.json(
        "POST",
        "/v1/fuses/runs/live-doomed-0/reset",
        Some(&json!({"grant": {"min_success_odds": 0.05}})),
        &[],
    );
    assert_eq!(status, 200, "{reset}");
    assert_eq!(reset["verdict"], "continue", "{reset}");

    // Refusals: limits that cannot mean anything.
    for (limits, needle) in [
        (json!({"min_success_odds": 1.5}), "probability in (0, 1]"),
        (json!({"min_success_odds": 0}), "probability in (0, 1]"),
        (
            json!({"foresight_min_support": 10}),
            "tune min_success_odds",
        ),
    ] {
        let (status, error) = server.json(
            "POST",
            "/v1/fuses/policies",
            Some(&json!({"name": "bad", "match": {"service": "x"}, "limits": limits})),
            &[],
        );
        assert_eq!(status, 400, "{error}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{error}"
        );
    }
}

#[test]
fn foresight_is_tenant_scoped_and_bound_credentials_cannot_cross() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:op-token,rw@acme:acme-rw,ro@acme:acme-ro,ro@beta:beta-ro",
        )],
    );
    let op = [("Authorization", "Bearer op-token")];
    let acme_rw = [("Authorization", "Bearer acme-rw")];
    let acme = [("Authorization", "Bearer acme-ro")];
    let beta = [("Authorization", "Bearer beta-ro")];
    ingest(&server, &corpus("", "", true), &op);
    let mut theirs = corpus("acme", "acme-", false);
    theirs.retain(|span| {
        span.attributes
            .get("session.id")
            .and_then(Value::as_str)
            .is_some_and(|run| run.starts_with("acme-search_loop"))
    });
    ingest(&server, &theirs, &acme_rw);

    let (status, listing) = get(&server, "/v1/fate/cohorts", &acme);
    assert_eq!(status, 200, "{listing}");
    let cohorts = listing["cohorts"].as_array().expect("cohorts");
    assert!(!cohorts.is_empty());
    assert!(
        cohorts.iter().all(|cohort| cohort["tenant"] == "acme"),
        "{listing}"
    );
    let (status, refused) = get(&server, "/v1/fate/cohorts?tenant=beta", &acme);
    assert_eq!(status, 403);
    assert_eq!(refused["required"], "unbound");
    let (status, refused) = get(
        &server,
        "/v1/fate/map?agent=support-agent&tenant=default",
        &acme,
    );
    assert_eq!(status, 403, "{refused}");

    let (status, map) = get(&server, "/v1/fate/map?agent=support-agent", &acme);
    assert_eq!(status, 200, "{map}");
    assert_eq!(
        (&map["tenant"], &map["runs"], &map["success"]),
        (&json!("acme"), &json!(50), &json!(5))
    );
    // Another tenant's run is not found, not merely refused.
    let (status, _) = get(&server, "/v1/fate/runs/live-doomed-0", &acme);
    assert_eq!(status, 404);
    let (status, own) = get(&server, "/v1/fate/runs/acme-search_loop-1", &acme);
    assert_eq!(status, 200, "{own}");
    assert_eq!(own["tenant"], "acme");
    // A tenant with no runs sees no cohorts at all.
    let (status, empty) = get(&server, "/v1/fate/cohorts", &beta);
    assert_eq!(status, 200);
    assert_eq!(empty["cohorts"], json!([]));

    // The unbound operator sees every tenant, apart.
    let (_, every) = get(&server, "/v1/fate/cohorts", &op);
    let support: Vec<(String, u64)> = every["cohorts"]
        .as_array()
        .expect("cohorts")
        .iter()
        .filter(|cohort| cohort["agent"] == "support-agent" && cohort.get("task").is_none())
        .map(|cohort| {
            (
                cohort["tenant"].as_str().unwrap_or_default().to_owned(),
                cohort["runs"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(support, [(String::new(), 180), ("acme".to_owned(), 50)]);
    let (_, acme_map) = get(&server, "/v1/fate/map?agent=support-agent&tenant=acme", &op);
    assert_eq!(acme_map["runs"], 50);
    let (_, default_map) = get(&server, "/v1/fate/map?agent=support-agent", &op);
    assert_eq!(default_map["runs"], 180);
}

#[test]
fn bounds_hold_say_so_and_bad_requests_are_refused() {
    let server = Server::spawn(&["--fate-max-runs", "20", "--fate-max-nodes", "6"]);
    ingest(&server, &corpus("", "", true), &[]);
    let map = ok(&server, "/v1/fate/map?agent=support-agent");
    assert_eq!(map["truncated"], true);
    assert_eq!(map["runs"], 20, "the newest 20 completed runs");
    assert!(map["bounds"]["runs_dropped"].as_u64().expect("dropped") > 0);
    assert_eq!(map["bounds"]["window_narrowed"], true, "{}", map["bounds"]);
    assert!(map["nodes_total"].as_u64().expect("nodes") <= 7);
    for (path, needle) in [
        (
            "/v1/fate/map?agent=support-agent&bogus=1",
            "unknown query parameter: bogus",
        ),
        ("/v1/fate/map?agent=support-agent&depth=0", "depth must be"),
        ("/v1/fate/map?agent=support-agent&depth=66", "depth must be"),
        (
            "/v1/fate/map?agent=support-agent&min_support=0",
            "min_support",
        ),
        ("/v1/fate/map?agent=support-agent&max_nodes=0", "max_nodes"),
        (
            "/v1/fate/map?agent=support-agent&since=10&until=5",
            "since must not be after until",
        ),
        (
            "/v1/fate/map?agent=support-agent&depth=-1",
            "non-negative integer",
        ),
        ("/v1/fate/map", "agent is required"),
        ("/v1/fate/map?agent=a&agent=b", "given twice"),
        ("/v1/fate/runs/live-doomed-0?min_support=0", "at least 1"),
        (
            "/v1/fate/cohorts?tenant=Not%20Valid",
            "tenant must be lowercase",
        ),
    ] {
        let (status, error) = get(&server, path, &[]);
        assert_eq!(status, 400, "{path}: {error}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{path}: {error}"
        );
    }
    let (status, _) = get(&server, "/v1/fate/runs/", &[]);
    assert_eq!(status, 404);
    let (status, _) = get(&server, "/v1/fate/nothing", &[]);
    assert_eq!(status, 404);
    let (status, _) = server.json("POST", "/v1/fate/map?agent=x", Some(&json!({})), &[]);
    assert_eq!(status, 404);
    // An explicit window is honoured: one that holds no runs has no cohort.
    let (status, _) = get(
        &server,
        "/v1/fate/map?agent=support-agent&since=1&until=2",
        &[],
    );
    assert_eq!(status, 404);
    let (status, cohorts) = get(&server, "/v1/fate/cohorts?since=1&until=2", &[]);
    assert_eq!(status, 200);
    assert_eq!(cohorts["cohorts"], json!([]));
}

#[test]
fn an_erasure_takes_a_run_out_of_every_cached_map() {
    let server = Server::spawn(&[]);
    ingest(&server, &corpus("", "", false), &[]);
    let before = ok(&server, "/v1/fate/map?agent=support-agent");
    let newest_failure = before["root"]["example_runs"][1]["run"]
        .as_str()
        .expect("an example")
        .to_owned();
    assert_eq!(newest_failure, "refund_happy-0");
    let (status, erased) = server.json(
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "session", "session_id": newest_failure}})),
        &[],
    );
    assert_eq!(status, 200, "{erased}");
    let after = ok(&server, "/v1/fate/map?agent=support-agent");
    assert_eq!(after["runs"], 179);
    assert_eq!(
        after["failure"],
        before["failure"].as_u64().expect("count") - 1
    );
    assert!(!after.to_string().contains("\"refund_happy-0\""), "{after}");
}

#[test]
fn mcp_foresee_run_answers_inside_the_telemetry_frame() {
    let server = Server::spawn(&["--mcp"]);
    ingest(&server, &corpus("", "", true), &[]);
    let mcp = |body: Value| {
        server.json(
            "POST",
            "/v1/mcp",
            Some(&body),
            &[("Accept", "application/json, text/event-stream")],
        )
    };
    let (status, listed) = mcp(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    assert_eq!(status, 200, "{listed}");
    let tool = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "foresee_run")
        .expect("foresee_run is listed")
        .clone();
    assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
    let (status, called) = mcp(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "foresee_run", "arguments": {"run_id": "live-doomed-0"}}}));
    assert_eq!(status, 200, "{called}");
    let result = &called["result"];
    assert_eq!(result["isError"], false, "{called}");
    assert_eq!(result["structuredContent"]["verdict"], "likely_failure");
    let text = result["content"][0]["text"].as_str().expect("text");
    let frame = text
        .find("<traza:telemetry untrusted=\"true\">")
        .expect("framed");
    assert!(text[..frame].contains("LIKELY FAILURE"), "{text}");
    assert!(!text[..frame].contains("search_orders"), "{text}");
    assert!(text[frame..].contains("search_orders"), "{text}");
}
