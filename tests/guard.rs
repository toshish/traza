//! Run fuses against the real server: every limit trips, warns come first,
//! stops stick until reset, the kill switch and grants work, the pre-call
//! check is hypothetical, tenants are isolated, the event feed resumes and
//! long-polls, OTLP clients get the header, a crash restart rebuilds runs
//! from the store. (The Claude Code hook pack that gates on all of it is
//! tested in `claude_code_hooks.rs`.)

mod common;

use std::time::{Duration, Instant};

use common::{header, Server};
use serde_json::{json, Value};

/// A tool step of `run`: same `tool` and `input` make the same fingerprint.
fn tool_step(run: &str, id: &str, tool: &str, input: &str, start: u64) -> Value {
    json!({
        "trace_id": format!("t-{run}"), "span_id": id, "name": tool, "service": "agent",
        "start_time_ns": start, "end_time_ns": start + 100,
        "attributes": {
            "session.id": run,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": tool,
            "gen_ai.tool.call.arguments": input,
        }
    })
}

/// An LLM step of `run` that metered `cost` dollars.
fn llm_step(run: &str, id: &str, cost: f64, start: u64) -> Value {
    json!({
        "trace_id": format!("t-{run}"), "span_id": id, "name": "chat", "service": "agent",
        "start_time_ns": start, "end_time_ns": start + 100,
        "attributes": {
            "session.id": run,
            "gen_ai.operation.name": "chat",
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.usage.input_tokens": 100,
            "gen_ai.usage.output_tokens": 20,
            "llm.cost_usd": cost,
        }
    })
}

/// `count` tool steps of `run`, each with its own input.
fn distinct_steps(run: &str, count: usize, first: usize) -> Value {
    Value::Array(
        (first..first + count)
            .map(|index| {
                tool_step(
                    run,
                    &format!("s{index}"),
                    "search",
                    &format!("{{\"q\":{index}}}"),
                    1_000 * index as u64,
                )
            })
            .collect(),
    )
}

/// Ingests `spans` natively; returns status, the `Traza-Fuse` header, body.
fn ingest(server: &Server, spans: &Value, auth: &[(&str, &str)]) -> (u16, Option<String>, Value) {
    let body = serde_json::to_vec(spans).expect("encodes");
    let (status, headers, text) = server.request("POST", "/v1/spans", Some(&body), auth);
    let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
    (
        status,
        header(&headers, "Traza-Fuse").map(str::to_owned),
        parsed,
    )
}

fn verdict(server: &Server, path: &str, auth: &[(&str, &str)]) -> Value {
    let (status, body) = server.json("GET", path, None, auth);
    assert_eq!(status, 200, "{path}: {body}");
    body
}

fn reasons(doc: &Value) -> Vec<String> {
    doc["reasons"]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .map(|reason| reason.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn each_limit_stops_the_run_that_reaches_it() {
    let server = Server::spawn(&[]);
    let (status, policy) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(&json!({
            "name": "default",
            "limits": {"max_cost_usd": 1.0, "max_steps": 10, "max_repeats": 3,
                       "max_error_streak": 3},
            "action": "stop",
        })),
        &[],
    );
    assert_eq!(status, 200, "{policy}");
    assert!(
        policy["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("fp_") && id.len() == 19),
        "{policy}"
    );

    // Cost: $0.60 is 60% of the budget, so nothing to report; the second
    // call spends it.
    let (_, header, body) = ingest(&server, &json!([llm_step("cost", "c1", 0.6, 1)]), &[]);
    assert!(body.get("fuses").is_none() && header.is_none(), "{body}");
    let (_, header, body) = ingest(&server, &json!([llm_step("cost", "c2", 0.6, 2)]), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=cost"));
    assert_eq!(
        body["fuses"],
        json!([{"run": "cost", "tenant": "", "verdict": "stop",
                "reasons": ["max_cost_usd 1 reached: $1.2 spent"]}])
    );

    // Steps: ten distinct calls.
    let (_, _, body) = ingest(&server, &distinct_steps("steps", 10, 0), &[]);
    assert_eq!(
        body["fuses"][0]["reasons"],
        json!(["max_steps 10 reached: 10 steps"])
    );

    // Repeats: the same call with the same input, three times.
    let looping: Vec<Value> = (0..3)
        .map(|index| {
            tool_step(
                "loop",
                &format!("l{index}"),
                "search",
                "{\"q\":\"same\"}",
                index,
            )
        })
        .collect();
    let (_, _, body) = ingest(&server, &Value::Array(looping), &[]);
    let reason = body["fuses"][0]["reasons"][0].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("max_repeats 3 reached: tool:search#")
            && reason.ends_with(" repeated 3 times"),
        "{body}"
    );

    // Error streak: three failed steps in a row stop the run...
    let failing = |run: &str, ids: &[&str], status: &[&str]| -> Value {
        Value::Array(
            ids.iter()
                .zip(status)
                .enumerate()
                .map(|(index, (id, status))| {
                    let mut step = tool_step(run, id, "fetch", &format!("{index}"), index as u64);
                    step["status"] = json!(status);
                    step
                })
                .collect(),
        )
    };
    let (_, _, body) = ingest(
        &server,
        &failing("streak", &["e1", "e2", "e3"], &["error", "error", "error"]),
        &[],
    );
    assert_eq!(
        body["fuses"][0]["reasons"],
        json!(["max_error_streak 3 reached: 3 consecutive failed steps"])
    );
    // ...while a success in between breaks the streak.
    let (_, header, body) = ingest(
        &server,
        &failing(
            "broken",
            &["e1", "e2", "ok", "e3", "e4"],
            &["error", "error", "ok", "error", "error"],
        ),
        &[],
    );
    assert!(header.is_none(), "a streak of two is 66%: {body}");
    // A retried batch moves nothing, the streak included: [ok, err, err]
    // sent twice is still a streak of two, so one more failure makes three.
    let batch = failing("retried", &["a", "b", "c"], &["ok", "error", "error"]);
    ingest(&server, &batch, &[]);
    let (_, header, _) = ingest(&server, &batch, &[]);
    assert!(header.is_none());
    let (_, header, _) = ingest(&server, &failing("retried", &["d"], &["error"]), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=retried"));

    // Every stop is on the run's own document, attributed to the policy.
    for run in ["cost", "steps", "loop", "streak"] {
        let doc = verdict(&server, &format!("/v1/fuses/runs/{run}"), &[]);
        assert_eq!(doc["verdict"], "stop", "{run}: {doc}");
        assert_eq!(doc["tripped_by"], "policy", "{run}");
        assert_eq!(doc["policy"], "default", "{run}");
    }
    let doc = verdict(&server, "/v1/fuses/runs/cost", &[]);
    assert_eq!(doc["usage"]["llm_calls"], 2);
    assert_eq!(doc["usage"]["tokens"], 240);
    assert_eq!(
        doc["evidence"],
        json!([{"trace_id": "t-cost", "span_id": "c2"}])
    );
    let doc = verdict(&server, "/v1/fuses/runs/loop", &[]);
    assert_eq!(doc["usage"]["top_repeat"]["count"], 3);
    assert_eq!(doc["usage"]["top_repeat"]["example"]["span_id"], "l2");
}

#[test]
fn a_run_is_warned_at_eighty_percent_before_it_is_stopped() {
    let server = Server::spawn(&["--fuse-max-steps", "5", "--fuse-action", "stop"]);
    let (status, header, body) = ingest(&server, &distinct_steps("w", 4, 0), &[]);
    assert_eq!(status, 200);
    assert_eq!(header.as_deref(), Some("warn; run=w"));
    assert_eq!(
        body,
        json!({"accepted": 4, "durability": "wal", "fuses": [
            {"run": "w", "tenant": "", "verdict": "warn",
             "reasons": ["max_steps 5 at 80%: 4 steps"]}]})
    );
    // An exporter retrying a batch the store already acknowledged sends the
    // same spans again: they are upserts, not four more steps.
    let (_, header, body) = ingest(&server, &distinct_steps("w", 4, 0), &[]);
    assert_eq!(header.as_deref(), Some("warn; run=w"), "{body}");
    assert_eq!(
        body["fuses"][0]["reasons"],
        json!(["max_steps 5 at 80%: 4 steps"])
    );
    let (_, header, _) = ingest(&server, &distinct_steps("w", 1, 4), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=w"));

    let doc = verdict(&server, "/v1/fuses/runs/w", &[]);
    assert_eq!(doc["policy"], "global");
    assert_eq!(doc["limits"], json!({"max_steps": 5, "max_repeats": 12}));
    assert_eq!(doc["grant"], Value::Null);

    // One warn and one stop, each announced once and counted once.
    let feed = verdict(&server, "/v1/fuses/events", &[]);
    let kinds: Vec<&str> = feed["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["warn", "stop"], "{feed}");
    let (_, _, metrics) = server.request("GET", "/v1/metrics", None, &[]);
    for line in [
        "traza_fuse_trips_total 1",
        "traza_fuse_warnings_total 1",
        "traza_fuse_live_runs 1",
    ] {
        assert!(
            metrics.lines().any(|metric| metric == line),
            "{line}:\n{metrics}"
        );
    }
}

#[test]
fn a_stop_sticks_until_reset_and_a_grant_raises_the_limit() {
    let server = Server::spawn(&["--fuse-max-steps", "3", "--fuse-action", "stop"]);
    ingest(&server, &distinct_steps("r", 3, 0), &[]);
    let stopped = verdict(&server, "/v1/fuses/runs/r", &[]);
    let tripped_at = stopped["tripped_at_ns"].as_u64().expect("tripped");

    // A later span of the run — not even a step — is still answered stop,
    // with the reasons and time it tripped with.
    let wrapper = json!([{"trace_id": "t-r", "span_id": "wrap", "name": "workflow",
                          "service": "agent", "start_time_ns": 1u64, "end_time_ns": 9_000u64,
                          "attributes": {"session.id": "r"}}]);
    let (_, header, _) = ingest(&server, &wrapper, &[]);
    assert_eq!(header.as_deref(), Some("stop; run=r"));
    let still = verdict(&server, "/v1/fuses/runs/r", &[]);
    assert_eq!(still["tripped_at_ns"].as_u64(), Some(tripped_at));
    assert_eq!(reasons(&still), ["max_steps 3 reached: 3 steps"]);

    // A reset without a grant cannot refill a spent budget: it stops again.
    let (status, doc) = server.json("POST", "/v1/fuses/runs/r/reset", None, &[]);
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["verdict"], "stop");
    assert_eq!(reasons(&doc), ["max_steps 3 reached: 3 steps"]);

    // A grant raises this run's limit, and the run continues against it.
    let (status, doc) = server.json(
        "POST",
        "/v1/fuses/runs/r/reset",
        Some(&json!({"grant": {"max_steps": 6}})),
        &[],
    );
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["verdict"], "continue", "{doc}");
    assert_eq!(doc["grant"], json!({"max_steps": 6}));
    assert_eq!(doc["limits"]["max_steps"], 6);
    assert_eq!(doc["tripped_at_ns"], Value::Null);
    let (_, header, _) = ingest(&server, &distinct_steps("r", 2, 3), &[]);
    assert_eq!(header.as_deref(), Some("warn; run=r"), "5 of 6");
    let (_, header, _) = ingest(&server, &distinct_steps("r", 1, 5), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=r"));

    // Grants are validated like policy limits, and bodies are closed.
    for (body, needle) in [
        (
            json!({"grant": {"max_steps": 0}}),
            "max_steps must be positive",
        ),
        (json!({"grant": {"max_stepz": 9}}), "unknown field"),
        (json!({"graant": {}}), "unknown field"),
    ] {
        let (status, error) = server.json("POST", "/v1/fuses/runs/r/reset", Some(&body), &[]);
        assert_eq!(status, 400, "{body}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{error}"
        );
    }

    // Raising the policy does not lift a sticky stop either.
    let (_, header, _) = ingest(&server, &wrapper, &[]);
    assert_eq!(header.as_deref(), Some("stop; run=r"));
}

#[test]
fn the_kill_switch_stops_any_run_until_it_is_reset() {
    let server = Server::spawn(&[]);
    let (status, doc) = server.json(
        "POST",
        "/v1/fuses/runs/k1/trip",
        Some(&json!({"reason": "runaway spend"})),
        &[],
    );
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["verdict"], "stop");
    assert_eq!(doc["tripped_by"], "manual");
    assert_eq!(reasons(&doc), ["manually tripped: runaway spend"]);

    let (_, check) = server.json("POST", "/v1/fuses/check", Some(&json!({"run": "k1"})), &[]);
    assert_eq!(check["verdict"], "stop");
    let (_, header, body) = ingest(&server, &distinct_steps("k1", 1, 0), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=k1"));
    assert_eq!(
        body["fuses"][0]["reasons"],
        json!(["manually tripped: runaway spend"])
    );
    let listed = verdict(&server, "/v1/fuses/runs?state=stop", &[]);
    assert_eq!(listed["runs"][0]["run"], "k1", "{listed}");

    let (status, doc) = server.json("POST", "/v1/fuses/runs/k1/reset", None, &[]);
    assert_eq!(status, 200);
    assert_eq!(doc["verdict"], "continue", "{doc}");
    let listed = verdict(&server, "/v1/fuses/runs?state=stop", &[]);
    assert_eq!(listed["runs"], json!([]));

    // A run id with a slash travels percent-encoded.
    let (status, doc) = server.json("POST", "/v1/fuses/runs/a%2Fb/trip", None, &[]);
    assert_eq!((status, doc["run"].as_str()), (200, Some("a/b")), "{doc}");

    // Paths that start with a route's prefix and end with its suffix, with
    // no id between: not found — and the server keeps serving.
    for path in [
        "/v1/fuses/runs/trip",
        "/v1/fuses/runs/reset",
        "/v1/fuses/policies/delete",
    ] {
        let (status, _) = server.json("POST", path, None, &[]);
        assert_eq!(status, 404, "{path}");
    }
    let still = verdict(&server, "/v1/fuses/runs/k1", &[]);
    assert_eq!(still["run"], "k1");

    let long_run = format!("/v1/fuses/runs/{}/trip", "r".repeat(1_025));
    for (path, body, needle) in [
        ("/v1/fuses/runs/trace:/trip", json!({}), "names no trace"),
        (
            "/v1/fuses/runs/k2/trip",
            json!({"reason": "x".repeat(2_000)}),
            "at most 1024 bytes",
        ),
        (
            long_run.as_str(),
            json!({}),
            "run id must be at most 1024 bytes",
        ),
    ] {
        let (status, error) = server.json("POST", path, Some(&body), &[]);
        assert_eq!(status, 400, "{path}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{error}"
        );
    }
}

#[test]
fn the_check_adds_the_next_step_hypothetically_and_changes_nothing() {
    let server = Server::spawn(&["--fuse-max-cost-usd", "1", "--fuse-action", "stop"]);
    ingest(&server, &json!([llm_step("c", "a", 0.5, 1)]), &[]);
    let check =
        |body: Value| -> (u16, Value) { server.json("POST", "/v1/fuses/check", Some(&body), &[]) };

    let (status, doc) = check(json!({"run": "c", "next": {"est_cost_usd": 0.3}}));
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["verdict"], "warn");
    assert_eq!(
        reasons(&doc),
        ["max_cost_usd 1 at 80%: $0.5 spent + $0.3 estimated"]
    );
    assert_eq!(doc["usage"]["cost_usd"], 0.5, "usage is what was recorded");

    let (status, doc) = check(json!({"run": "c", "next": {"est_cost_usd": 0.6, "tool": "x"}}));
    assert_eq!(status, 200, "a stop verdict is still a 200: {doc}");
    assert_eq!(doc["verdict"], "stop");
    assert_eq!(
        reasons(&doc),
        ["max_cost_usd 1 would be exceeded by the next step: $0.5 spent + $0.6 estimated"]
    );
    assert_eq!(
        doc["tripped_by"],
        Value::Null,
        "a hypothetical trips nothing"
    );

    // Nothing stuck: the run itself is fine.
    let doc = verdict(&server, "/v1/fuses/runs/c", &[]);
    assert_eq!(doc["verdict"], "continue", "{doc}");
    let (_, doc) = check(json!({"run": "c"}));
    assert_eq!(doc["verdict"], "continue");

    // A fresh run may start — unless its first step alone is over budget.
    let (_, doc) = check(json!({"run": "fresh", "next": {"tool": "search"}}));
    assert_eq!(
        (doc["verdict"].as_str(), doc["usage"]["steps"].as_u64()),
        (Some("continue"), Some(0))
    );
    let (_, doc) = check(json!({"run": "fresh", "next": {"est_cost_usd": 2.0}}));
    assert_eq!(doc["verdict"], "stop");

    for (body, needle) in [
        (
            json!({"run": "c", "next": {"est_cost_usd": -1}}),
            "non-negative",
        ),
        (json!({"next": {}}), "missing field `run`"),
        (json!({"run": "c", "nxt": {}}), "unknown field"),
        (json!({"run": ""}), "run id is empty"),
    ] {
        let (status, error) = check(body.clone());
        assert_eq!(status, 400, "{body}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{error}"
        );
    }
}

#[test]
fn policies_and_verdicts_are_per_tenant_and_bound_tokens_cannot_cross() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:op-token,rw@acme:acme-rw,ro@acme:acme-ro,rw@beta:beta-rw",
        )],
    );
    let op = [("Authorization", "Bearer op-token")];
    let acme = [("Authorization", "Bearer acme-rw")];
    let acme_ro = [("Authorization", "Bearer acme-ro")];
    let beta = [("Authorization", "Bearer beta-rw")];

    let (status, policy) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(&json!({"name": "default", "limits": {"max_steps": 2}, "action": "stop"})),
        &acme,
    );
    assert_eq!(status, 200, "{policy}");
    assert_eq!(
        policy["tenant"], "acme",
        "a bound caller writes its own tenant"
    );
    let foreign = json!({"name": "x", "tenant": "beta", "match": {"service": "s"}, "limits": {}});
    let (status, _) = server.json("POST", "/v1/fuses/policies", Some(&foreign), &acme);
    assert_eq!(status, 403);
    // Naming no tenant, so the missing scope is the only reason to refuse.
    let own = json!({"name": "x", "match": {"service": "s"}, "limits": {}});
    let (status, _) = server.json("POST", "/v1/fuses/policies", Some(&own), &acme_ro);
    assert_eq!(status, 403, "a read-only token cannot write policy");
    let (status, written) = server.json("POST", "/v1/fuses/policies", Some(&own), &acme);
    assert_eq!(status, 200, "the same body with write scope: {written}");

    let listed = verdict(&server, "/v1/fuses/policies", &beta);
    assert_eq!(listed["policies"], json!([]), "beta sees none of acme's");
    assert_eq!(listed["global"]["name"], "global");
    let listed = verdict(&server, "/v1/fuses/policies", &op);
    assert_eq!(listed["policies"][0]["id"], policy["id"]);
    let listed = verdict(&server, "/v1/fuses/policies?tenant=beta", &op);
    assert_eq!(listed["policies"], json!([]));

    // The same run id in two tenants is two runs under two policies.
    let (_, header, _) = ingest(&server, &distinct_steps("shared", 2, 0), &acme);
    assert_eq!(header.as_deref(), Some("stop; run=shared"));
    let (_, header, _) = ingest(&server, &distinct_steps("shared", 2, 0), &beta);
    assert_eq!(header, None);

    let doc = verdict(&server, "/v1/fuses/runs/shared", &beta);
    assert_eq!(
        (doc["tenant"].as_str(), doc["verdict"].as_str()),
        (Some("beta"), Some("continue"))
    );
    let (status, _) = server.json("GET", "/v1/fuses/runs/shared?tenant=acme", None, &beta);
    assert_eq!(status, 403);
    let doc = verdict(&server, "/v1/fuses/runs/shared?tenant=acme", &op);
    assert_eq!(doc["verdict"], "stop");
    let doc = verdict(&server, "/v1/fuses/runs/shared", &op);
    assert_eq!(
        (doc["tenant"].as_str(), doc["usage"]["steps"].as_u64()),
        (Some(""), Some(0)),
        "an unbound caller naming no tenant asks the default tenant"
    );

    // A read-only token may ask before a step, and nothing more.
    let (status, doc) = server.json(
        "POST",
        "/v1/fuses/check",
        Some(&json!({"run": "shared", "next": {"tool": "search"}})),
        &acme_ro,
    );
    assert_eq!(
        (status, doc["verdict"].as_str()),
        (200, Some("stop")),
        "{doc}"
    );
    let (status, _) = server.json("POST", "/v1/fuses/runs/shared/trip", None, &acme_ro);
    assert_eq!(status, 403);

    let id = policy["id"].as_str().expect("id");
    let (status, _) = server.json(
        "POST",
        &format!("/v1/fuses/policies/{id}/delete"),
        None,
        &beta,
    );
    assert_eq!(
        status, 404,
        "another tenant's policy does not exist for beta"
    );

    let stopped = verdict(&server, "/v1/fuses/runs?state=stop", &beta);
    assert_eq!(stopped["runs"], json!([]));
    let stopped = verdict(&server, "/v1/fuses/runs?state=stop", &op);
    assert_eq!(stopped["runs"][0]["tenant"], "acme", "{stopped}");
    let events = verdict(&server, "/v1/fuses/events", &beta);
    assert_eq!(events["events"], json!([]));
    let events = verdict(&server, "/v1/fuses/events?tenant=acme", &op);
    assert_eq!(events["events"][0]["run"], "shared");
    let (status, _) = server.json("GET", "/v1/fuses/events?tenant=acme", None, &beta);
    assert_eq!(status, 403);

    // A service-matched policy beats the tenant default for runs whose
    // first span came from that service.
    let (status, search) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(
            &json!({"name": "search", "match": {"service": "search-svc"},
                     "limits": {"max_steps": 50}}),
        ),
        &acme,
    );
    assert_eq!(status, 200, "{search}");
    assert_eq!(search["action"], "warn", "the default action");
    let mut step = tool_step("svc-run", "s0", "search", "{}", 1);
    step["service"] = json!("search-svc");
    ingest(&server, &json!([step]), &acme);
    let doc = verdict(&server, "/v1/fuses/runs/svc-run", &acme);
    assert_eq!(doc["policy"], "search");
    assert_eq!(
        doc["limits"],
        json!({"max_steps": 50}),
        "no inheritance from default"
    );
    let search_id = search["id"].as_str().expect("id");
    let (status, deleted) = server.json(
        "POST",
        &format!("/v1/fuses/policies/{search_id}/delete"),
        None,
        &acme,
    );
    assert_eq!((status, deleted["deleted"].as_bool()), (200, Some(true)));
    let doc = verdict(&server, "/v1/fuses/runs/svc-run", &acme);
    assert_eq!(doc["policy"], "default");
}

#[test]
fn events_resume_by_cursor_wake_a_long_poll_and_report_a_gap() {
    let mut server = Server::spawn(&[]);
    let head = verdict(&server, "/v1/fuses/events", &[]);
    assert_eq!(head["events"], json!([]));
    let cursor = head["next"].as_str().expect("cursor").to_owned();
    assert!(cursor.ends_with(".0"), "{cursor}");

    let (page, waited) = std::thread::scope(|scope| {
        let poll = scope.spawn(|| {
            let started = Instant::now();
            let page = verdict(
                &server,
                &format!("/v1/fuses/events?after={cursor}&wait_ms=20000"),
                &[],
            );
            (page, started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(300));
        server.json("POST", "/v1/fuses/runs/e1/trip", None, &[]);
        poll.join().expect("poll joins")
    });
    assert_eq!(page["events"][0]["kind"], "manual_stop", "{page}");
    assert_eq!(page["events"][0]["run"], "e1");
    // The wait ended on the event, not on the 20 s bound.
    assert!(waited < Duration::from_secs(15), "{waited:?}");
    let next = page["next"].as_str().expect("next").to_owned();
    assert_eq!(page["events"][0]["cursor"], next);

    let quiet = verdict(
        &server,
        &format!("/v1/fuses/events?after={next}&wait_ms=200"),
        &[],
    );
    assert_eq!(quiet["events"], json!([]));
    assert_eq!(quiet["next"], next, "an empty read holds its position");
    assert!(quiet.get("gap").is_none());

    for (path, needle) in [
        ("/v1/fuses/events?after=garbage", "<epoch>.<seq>"),
        ("/v1/fuses/events?wait_ms=30001", "wait_ms must be 0..30000"),
        ("/v1/fuses/events?bogus=1", "unknown query parameter: bogus"),
    ] {
        let (status, error) = server.json("GET", path, None, &[]);
        assert_eq!(status, 400, "{path}");
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{error}"
        );
    }

    // A cursor from before a restart names another process's numbering: the
    // reader is told it may have missed events and given what survives.
    server.restart();
    server.json("POST", "/v1/fuses/runs/e2/trip", None, &[]);
    let resumed = verdict(&server, &format!("/v1/fuses/events?after={next}"), &[]);
    assert_eq!(resumed["gap"], true, "{resumed}");
    let runs: Vec<&str> = resumed["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["run"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(runs, ["e2"]);
}

/// A minimal OTLP protobuf export of one span.
fn otlp_protobuf(session: &str) -> Vec<u8> {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    fn fixed64(number: u64, value: u64, out: &mut Vec<u8>) {
        varint(number << 3 | 1, out);
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn attribute(key: &str, value: &str) -> Vec<u8> {
        let mut any = Vec::new();
        field(1, value.as_bytes(), &mut any);
        let mut pair = Vec::new();
        field(1, key.as_bytes(), &mut pair);
        field(2, &any, &mut pair);
        pair
    }
    let mut span = Vec::new();
    field(1, &[0xCC; 16], &mut span);
    field(2, &[0x07; 8], &mut span);
    field(5, b"chat", &mut span);
    fixed64(7, 1_700_000_000_000_000_000, &mut span);
    fixed64(8, 1_700_000_001_000_000_000, &mut span);
    field(9, &attribute("session.id", session), &mut span);
    field(9, &attribute("gen_ai.operation.name", "chat"), &mut span);
    let mut scope_spans = Vec::new();
    field(2, &span, &mut scope_spans);
    let mut resource_spans = Vec::new();
    field(2, &scope_spans, &mut resource_spans);
    let mut request = Vec::new();
    field(1, &resource_spans, &mut request);
    request
}

#[test]
fn otlp_ingest_carries_the_verdict_in_a_header_and_leaves_the_body_alone() {
    let server = Server::spawn(&["--fuse-max-steps", "1", "--fuse-action", "stop"]);
    let otlp_json = |session: &str, operation: Option<&str>| -> Vec<u8> {
        let mut attributes = vec![json!({"key": "session.id", "value": {"stringValue": session}})];
        if let Some(operation) = operation {
            attributes.push(json!({"key": "gen_ai.operation.name",
                                   "value": {"stringValue": operation}}));
        }
        serde_json::to_vec(&json!({"resourceSpans": [{"scopeSpans": [{"spans": [{
            "traceId": "dddddddddddddddddddddddddddddddd", "spanId": "eeeeeeeeeeeeeeee",
            "name": "chat", "startTimeUnixNano": "1700000000000000000",
            "endTimeUnixNano": "1700000001000000000", "attributes": attributes,
        }]}]}]}))
        .expect("encodes")
    };

    let (status, headers, body) = server.request(
        "POST",
        "/v1/traces",
        Some(&otlp_json("otlp-run", Some("chat"))),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(header(&headers, "Traza-Fuse"), Some("stop; run=otlp-run"));
    assert_eq!(
        body, r#"{"partialSuccess":{}}"#,
        "the OTLP body is unchanged"
    );

    let (status, headers, body) = server.request(
        "POST",
        "/v1/traces",
        Some(&otlp_protobuf("pb-run")),
        &[("Content-Type", "application/x-protobuf")],
    );
    assert_eq!(status, 200);
    assert!(body.is_empty(), "full success is zero protobuf bytes");
    assert_eq!(header(&headers, "Traza-Fuse"), Some("stop; run=pb-run"));

    // A batch with nothing to report carries no header at all.
    let (_, headers, _) =
        server.request("POST", "/v1/traces", Some(&otlp_json("quiet", None)), &[]);
    assert_eq!(header(&headers, "Traza-Fuse"), None);

    // A run id is telemetry: it cannot forge a header or a parameter.
    let (_, headers, _) = server.request(
        "POST",
        "/v1/traces",
        Some(&otlp_json("evil run;\r\nX-Injected: 1", Some("chat"))),
        &[],
    );
    assert_eq!(
        header(&headers, "Traza-Fuse"),
        Some("stop; run=evil%20run%3B%0D%0AX-Injected:%201")
    );
    assert_eq!(header(&headers, "X-Injected"), None);
}

#[test]
fn a_crash_restart_rebuilds_runs_from_the_store_and_keeps_durable_controls() {
    let mut server = Server::spawn(&["--fuse-max-repeats", "3", "--fuse-action", "stop"]);
    let same = |run: &str, id: &str, start: u64| tool_step(run, id, "search", "{\"q\":1}", start);
    ingest(
        &server,
        &json!([
            same("loop", "a", 1_000),
            same("loop", "b", 2_000),
            same("loop", "c", 3_000)
        ]),
        &[],
    );
    ingest(
        &server,
        &json!([same("resume", "a", 1), same("resume", "b", 2)]),
        &[],
    );
    server.json(
        "POST",
        "/v1/fuses/runs/held/trip",
        Some(&json!({"reason": "on hold"})),
        &[],
    );
    let (_, policy) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(&json!({"name": "billing", "match": {"service": "billing"},
                     "limits": {"max_steps": 2}, "action": "stop"})),
        &[],
    );

    server.restart();

    let doc = verdict(&server, "/v1/fuses/runs/loop", &[]);
    assert_eq!(doc["source"], "rebuilt", "{doc}");
    assert_eq!(doc["verdict"], "stop");
    assert_eq!(doc["tripped_by"], "policy");
    assert_eq!(doc["usage"]["steps"], 3);
    assert_eq!(doc["usage"]["top_repeat"]["count"], 3);
    // Recomputed by replay: the stop is timed by the span that crossed it.
    assert_eq!(doc["tripped_at_ns"], 3_100);
    assert_eq!(
        verdict(&server, "/v1/fuses/runs/loop", &[])["source"],
        "live"
    );

    // The kill switch and the policy are durable.
    let held = verdict(&server, "/v1/fuses/runs/held", &[]);
    assert_eq!(reasons(&held), ["manually tripped: on hold"]);
    let stopped = verdict(&server, "/v1/fuses/runs?state=stop", &[]);
    let runs: Vec<&str> = stopped["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|doc| doc["run"].as_str().unwrap_or_default())
        .collect();
    assert!(
        runs.contains(&"held") && runs.contains(&"loop"),
        "{stopped}"
    );
    let policies = verdict(&server, "/v1/fuses/policies", &[]);
    assert_eq!(policies["policies"][0]["id"], policy["id"]);

    // A run continuing after the restart: the pre-call check rebuilds its
    // history, so the next acknowledgement judges the whole run.
    let (_, doc) = server.json(
        "POST",
        "/v1/fuses/check",
        Some(&json!({"run": "resume", "next": {"tool": "search"}})),
        &[],
    );
    assert_eq!(
        (doc["source"].as_str(), doc["usage"]["steps"].as_u64()),
        (Some("rebuilt"), Some(2))
    );
    let (_, header, _) = ingest(&server, &json!([same("resume", "c", 3)]), &[]);
    assert_eq!(header.as_deref(), Some("stop; run=resume"));
}

#[test]
fn a_batch_moving_cost_between_spans_stops_nothing_live_or_after_a_restart() {
    let mut server = Server::spawn(&["--fuse-max-cost-usd", "10", "--fuse-action", "stop"]);
    let cost = |doc: &Value| doc["usage"]["cost_usd"].as_f64();
    // A complete run at $7 of $10: B ($1) started first, A ($6) after.
    ingest(
        &server,
        &json!([
            llm_step("xfer", "b", 1.0, 10),
            llm_step("xfer", "a", 6.0, 20)
        ]),
        &[],
    );
    let doc = verdict(&server, "/v1/fuses/runs/xfer", &[]);
    assert_eq!(
        (doc["source"].as_str(), doc["verdict"].as_str(), cost(&doc)),
        (Some("rebuilt"), Some("continue"), Some(7.0)),
        "{doc}"
    );

    // One batch re-sends both, moving $5 from A to B, A first. The store
    // goes from $7 to $7 and never holds more.
    let (status, header, body) = ingest(
        &server,
        &json!([
            llm_step("xfer", "a", 1.0, 20),
            llm_step("xfer", "b", 6.0, 10)
        ]),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert!(header.is_none() && body.get("fuses").is_none(), "{body}");
    let doc = verdict(&server, "/v1/fuses/runs/xfer", &[]);
    assert_eq!(
        (doc["source"].as_str(), doc["verdict"].as_str(), cost(&doc)),
        (Some("live"), Some("continue"), Some(7.0)),
        "{doc}"
    );
    assert_eq!(doc["tripped_at_ns"], Value::Null, "{doc}");

    // A replay of the store after a restart says the same.
    server.restart();
    let doc = verdict(&server, "/v1/fuses/runs/xfer", &[]);
    assert_eq!(
        (doc["source"].as_str(), doc["verdict"].as_str(), cost(&doc)),
        (Some("rebuilt"), Some("continue"), Some(7.0)),
        "{doc}"
    );
}

#[test]
fn no_fuses_turns_every_surface_off() {
    let server = Server::spawn(&[
        "--no-fuses",
        "--mcp",
        "--fuse-max-steps",
        "1",
        "--fuse-action",
        "stop",
    ]);
    let (status, header, body) = ingest(&server, &distinct_steps("n", 2, 0), &[]);
    assert_eq!(status, 200);
    assert_eq!(header, None);
    assert_eq!(body, json!({"accepted": 2, "durability": "wal"}));
    for (method, path) in [("GET", "/v1/fuses/runs/n"), ("POST", "/v1/fuses/check")] {
        let (status, error) = server.json(method, path, Some(&json!({"run": "n"})), &[]);
        assert_eq!(status, 409, "{path}");
        assert!(
            error["error"]
                .as_str()
                .unwrap_or_default()
                .contains("--no-fuses"),
            "{error}"
        );
    }
    let (_, _, metrics) = server.request("GET", "/v1/metrics", None, &[]);
    assert!(!metrics.contains("traza_fuse_"), "{metrics}");
    // MCP neither lists the guard tool nor answers it.
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
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        !names.is_empty() && !names.contains(&"check_run_guard"),
        "{names:?}"
    );
    let (_, called) = mcp(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "check_run_guard", "arguments": {"run_id": "n"}}}));
    assert!(
        called["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("--no-fuses"),
        "{called}"
    );
    assert!(
        server.stderr().contains("run fuses off"),
        "{}",
        server.stderr()
    );
    assert!(!server.data_dir.join("fuses.jsonl").exists());
}

#[test]
fn mcp_check_run_guard_reports_the_verdict_inside_the_telemetry_frame() {
    let server = Server::spawn(&["--mcp", "--fuse-max-steps", "1", "--fuse-action", "stop"]);
    ingest(&server, &distinct_steps("m1", 1, 0), &[]);
    let (status, response) = server.json(
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": {"name": "check_run_guard", "arguments": {"run_id": "m1"}}})),
        &[("Accept", "application/json, text/event-stream")],
    );
    assert_eq!(status, 200, "{response}");
    let result = &response["result"];
    assert_eq!(result["structuredContent"]["verdict"], "stop", "{result}");
    assert_eq!(result["structuredContent"]["usage"]["steps"], 1);
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("Run guard: STOP."), "{text}");
    let frame = text
        .split_once("<traza:telemetry untrusted=\"true\">")
        .map(|(_, rest)| rest)
        .expect("a telemetry frame");
    assert!(
        frame.contains("run m1") && frame.contains("max_steps 1 reached"),
        "{text}"
    );
}
