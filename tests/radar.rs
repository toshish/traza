//! The change radar against the real `traza-server` binary: OTLP spans whose
//! resource carries the deploy, `GET /v1/changes` finding the regression,
//! tenant isolation under bound credentials, the refusals, the run cap, the
//! query deadline, and `what_changed` over MCP.
//!
//! Every run is synthetic and deterministic: 60 runs on one configuration,
//! then 60 on another, a minute apart, two days before the test's clock (so
//! every run is finished by the session-idle rule), failing on a fixed
//! pattern.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::Server;
use serde_json::{json, Value};

const MINUTE: u64 = 60_000_000_000;
const DAY: u64 = 86_400_000_000_000;

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

/// One era of runs as an OTLP/HTTP JSON request: one `ResourceSpans` whose
/// resource carries the service, its version, the SDK, the environment and
/// (optionally) the tenant.
struct Era<'a> {
    tenant: Option<&'a str>,
    first: usize,
    last: usize,
    version: &'a str,
    prompt: &'a str,
    fails: fn(usize) -> bool,
}

impl Era<'_> {
    fn otlp(&self, base: u64) -> Value {
        let mut resource = vec![
            attribute("service.name", "support-bot"),
            attribute("service.version", self.version),
            attribute("telemetry.sdk.name", "opentelemetry"),
            attribute("telemetry.sdk.version", "1.27.0"),
            attribute("deployment.environment.name", "prod"),
        ];
        if let Some(tenant) = self.tenant {
            resource.push(attribute("traza.tenant", tenant));
        }
        let messages = json!([
            {"role": "system", "content": self.prompt},
            {"role": "user", "content": "My refund never arrived."}
        ])
        .to_string();
        let mut spans = Vec::new();
        for index in self.first..self.last {
            let start = base + index as u64 * MINUTE;
            for (span, offset, failed) in [(1u64, 0u64, false), (2, 500, (self.fails)(index))] {
                spans.push(json!({
                    "traceId": format!("{:032x}", index + 1),
                    "spanId": format!("{span:016x}"),
                    "name": "chat gpt-4o",
                    "startTimeUnixNano": (start + offset).to_string(),
                    "endTimeUnixNano": (start + offset + 5).to_string(),
                    "status": {"code": if failed { 2 } else { 1 }},
                    "attributes": [
                        attribute("session.id", &format!("ticket-{index:03}")),
                        attribute("gen_ai.operation.name", "chat"),
                        attribute("gen_ai.request.model", "gpt-4o"),
                        attribute("gen_ai.input.messages", &messages),
                    ],
                }));
            }
        }
        json!({"resourceSpans": [{
            "resource": {"attributes": resource},
            "scopeSpans": [{"spans": spans}],
        }]})
    }
}

fn rarely(index: usize) -> bool {
    index % 20 == 0
}

fn often(index: usize) -> bool {
    index % 5 < 2
}

/// Ingests the two eras of the regression: a prompt edit shipped with
/// version 1.5.0, after which two runs in five fail instead of one in twenty.
fn ingest_regression(server: &Server, tenant: Option<&str>, base: u64, token: Option<&str>) {
    let authorization = token.map(|token| format!("Bearer {token}"));
    let mut headers = vec![("Content-Type", "application/json")];
    if let Some(value) = authorization.as_deref() {
        headers.push(("Authorization", value));
    }
    for era in [
        Era {
            tenant,
            first: 0,
            last: 60,
            version: "1.4.2",
            prompt: "You triage tickets. Always escalate refunds to a human.",
            fails: rarely,
        },
        Era {
            tenant,
            first: 60,
            last: 120,
            version: "1.5.0",
            prompt: "You triage tickets. Never escalate refunds to a human.",
            fails: often,
        },
    ] {
        let (status, body) = server.json("POST", "/v1/traces", Some(&era.otlp(base)), &headers);
        assert_eq!(status, 200, "{body}");
    }
}

/// The same 120 runs with nothing changing, for a tenant that must stay
/// quiet.
fn ingest_quiet(server: &Server, tenant: &str, base: u64, token: &str) {
    let era = Era {
        tenant: Some(tenant),
        first: 0,
        last: 120,
        version: "2.0.0",
        prompt: "Quiet prompt.",
        fails: rarely,
    };
    let authorization = format!("Bearer {token}");
    let (status, body) = server.json(
        "POST",
        "/v1/traces",
        Some(&era.otlp(base)),
        &[("Authorization", authorization.as_str())],
    );
    assert_eq!(status, 200, "{body}");
}

fn window(base: u64) -> String {
    format!("since={}&until={}", base - MINUTE, now_ns())
}

fn change<'a>(report: &'a Value, dimension: &str) -> Option<&'a Value> {
    report["agents"]
        .as_array()?
        .iter()
        .flat_map(|agent| agent["changes"].as_array().into_iter().flatten())
        .find(|change| change["dimension"] == json!(dimension))
}

#[test]
fn a_deploy_shipped_over_otlp_is_found_ranked_and_explained() {
    let server = Server::spawn(&[]);
    let base = now_ns() - 2 * DAY;
    ingest_regression(&server, None, base, None);

    // The resource's build attributes reached the stored span.
    let (status, trace) = server.json("GET", &format!("/v1/traces/{:032x}", 1), None, &[]);
    assert_eq!(status, 200, "{trace}");
    let attributes = &trace["spans"][0]["attributes"];
    assert_eq!(attributes["service.version"], "1.4.2");
    assert_eq!(attributes["telemetry.sdk.name"], "opentelemetry");
    assert_eq!(attributes["deployment.environment.name"], "prod");

    let (status, report) = server.json("GET", &format!("/v1/changes?{}", window(base)), None, &[]);
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["truncated"], json!(false));
    assert_eq!(report["runs_scanned"], json!(120));
    let agent = &report["agents"][0];
    assert_eq!(agent["agent"], "support-bot");
    assert_eq!(agent["runs"], json!(120));
    assert_eq!(agent["current"]["service_version"], "1.5.0");
    assert_eq!(agent["current"]["sdk_version"], "opentelemetry/1.27.0");
    assert_eq!(agent["changes"][0]["verdict"], "regression", "{agent}");
    assert!(
        agent["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("regression at"),
        "{agent}"
    );

    let deploy = change(&report, "service_version").expect("the deploy is a change point");
    assert_eq!(
        (&deploy["from"], &deploy["to"]),
        (&json!("1.4.2"), &json!("1.5.0"))
    );
    assert_eq!(deploy["at_ns"], json!(base + 60 * MINUTE));
    assert_eq!(deploy["co_changes"], json!(["system_prompt"]));
    assert_eq!(deploy["verdict"], "regression");
    let errors = &deploy["deltas"]["error_rate"];
    assert_eq!(errors["before"], json!(0.05));
    assert_eq!(errors["after"], json!(0.4));
    assert!(
        errors["p_value"].as_f64().is_some_and(|p| p < 0.001),
        "{errors}"
    );
    assert_eq!(deploy["runs_before"], json!(60));
    assert_eq!(deploy["runs_after"], json!(60));

    let prompt = change(&report, "system_prompt").expect("the prompt edit is a change point");
    assert_eq!(prompt["detail"]["diff"]["before"], "Always");
    assert_eq!(prompt["detail"]["diff"]["after"], "Never");
    assert_eq!(prompt["example_runs"]["after"][0], "ticket-060");

    // One failed run, placed on the timeline: it ran on both changed values.
    let (status, placed) = server.json(
        "GET",
        &format!("/v1/changes?{}&run=ticket-076", window(base)),
        None,
        &[],
    );
    assert_eq!(status, 200, "{placed}");
    assert_eq!(placed["run"]["outcome"], "failure");
    assert_eq!(placed["run"]["fingerprint"]["service_version"], "1.5.0");
    let downstream: Vec<&str> = placed["run"]["downstream_of"]
        .as_array()
        .expect("placed")
        .iter()
        .map(|change| change["dimension"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(downstream, ["system_prompt", "service_version"]);
    let (status, missing) = server.json(
        "GET",
        &format!("/v1/changes?{}&run=ticket-999", window(base)),
        None,
        &[],
    );
    assert_eq!(status, 404, "{missing}");

    // Filters narrow the cohort without changing the answer's shape.
    for (query, agents) in [
        ("environment=prod", 1),
        ("environment=staging", 0),
        ("agent=support-bot", 1),
        ("agent=billing-bot", 0),
    ] {
        let (status, filtered) = server.json(
            "GET",
            &format!("/v1/changes?{}&{query}", window(base)),
            None,
            &[],
        );
        assert_eq!(status, 200, "{filtered}");
        assert_eq!(
            filtered["agents"].as_array().map(Vec::len),
            Some(agents),
            "{query}"
        );
    }
}

#[test]
fn bound_credentials_see_only_their_tenant() {
    let tokens = "admin:op-token,ro@acme:acme-ro,ro@globex:globex-ro";
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", tokens)]);
    let base = now_ns() - 2 * DAY;
    ingest_regression(&server, Some("acme"), base, Some("op-token"));
    ingest_quiet(&server, "globex", base, "op-token");
    let get = |token: &str, extra: &str| {
        let authorization = format!("Bearer {token}");
        server.json(
            "GET",
            &format!("/v1/changes?{}{extra}", window(base)),
            None,
            &[("Authorization", authorization.as_str())],
        )
    };

    let (status, acme) = get("acme-ro", "");
    assert_eq!(status, 200, "{acme}");
    assert_eq!(acme["agents"].as_array().map(Vec::len), Some(1));
    assert_eq!(acme["agents"][0]["tenant"], "acme");
    assert!(change(&acme, "system_prompt").is_some());
    let acme_prompt = change(&acme, "system_prompt").unwrap()["to"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, globex) = get("globex-ro", "");
    assert_eq!(status, 200, "{globex}");
    assert_eq!(globex["agents"].as_array().map(Vec::len), Some(1));
    assert_eq!(globex["agents"][0]["tenant"], "globex");
    assert_eq!(globex["agents"][0]["changes"], json!([]));
    let text = globex.to_string();
    assert!(
        !text.contains("acme") && !text.contains(&acme_prompt),
        "{text}"
    );

    let (status, refused) = get("acme-ro", "&tenant=globex");
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["required"], "unbound");
    let (status, _) = get("acme-ro", "&tenant=acme");
    assert_eq!(status, 200);

    let (status, operator) = get("op-token", "");
    assert_eq!(status, 200);
    assert_eq!(operator["agents"].as_array().map(Vec::len), Some(2));
    let (status, scoped) = get("op-token", "&tenant=globex");
    assert_eq!(status, 200);
    assert_eq!(scoped["agents"][0]["tenant"], "globex");

    let (status, _) = server.json("GET", "/v1/changes", None, &[]);
    assert_eq!(status, 401, "no credential, no answer");
    let (status, _) = server.json(
        "POST",
        "/v1/changes",
        None,
        &[("Authorization", "Bearer op-token")],
    );
    assert_eq!(status, 404, "the route is a read");
}

#[test]
fn bad_questions_are_refused_and_the_run_cap_is_stated() {
    let server = Server::spawn(&["--radar-max-runs", "20"]);
    let base = now_ns() - 2 * DAY;
    for (query, fragment) in [
        ("bogus=1", "unknown query parameter: bogus"),
        ("since=yesterday", "since must be Unix nanoseconds"),
        ("since=20&until=10", "since must not be after until"),
        ("behavior_threshold=5", "behavior_threshold"),
        ("agent=a&agent=b", "given twice"),
        ("tenant=Not+A+Tenant", "tenant must be"),
    ] {
        let (status, body) = server.json("GET", &format!("/v1/changes?{query}"), None, &[]);
        assert_eq!(status, 400, "{query}: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains(fragment),
            "{query}: {body}"
        );
    }

    ingest_regression(&server, None, base, None);
    let (status, report) = server.json("GET", &format!("/v1/changes?{}", window(base)), None, &[]);
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["truncated"], json!(true));
    let mark = report["covered_since_ns"]
        .as_u64()
        .expect("the cut is stated");
    let kept = (0..120u64)
        .filter(|index| base + index * MINUTE >= mark)
        .count();
    assert_eq!(report["runs_scanned"], json!(kept));
    assert_eq!(kept, 20, "exactly the cap: the newest twenty runs, whole");
    assert_eq!(mark, base + 99 * MINUTE + 1);

    // A run the filters exclude is refused by name, not "not found".
    let (status, body) = server.json(
        "GET",
        &format!(
            "/v1/changes?{}&run=ticket-110&agent=billing-bot",
            window(base)
        ),
        None,
        &[],
    );
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("excluded by the agent filter"),
        "{body}"
    );

    // A cap of zero is refused at startup, where the operator sees it.
    let dir = std::env::temp_dir().join(format!("traza-radar-cap-{}", std::process::id()));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .args(["--radar-max-runs", "0", "--port", "0", "--data-dir"])
        .arg(&dir)
        .output()
        .expect("runs");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--radar-max-runs must be at least 1"));
}

#[test]
fn copied_build_attributes_never_push_an_export_over_the_attribute_cap() {
    // Each span carries exactly four attributes of its own; the resource
    // carries the five build attributes that are copied onto it.
    let server = Server::spawn(&["--max-span-attributes", "4"]);
    let base = now_ns() - 2 * DAY;
    let era = Era {
        tenant: None,
        first: 0,
        last: 3,
        version: "1.4.2",
        prompt: "Prompt.",
        fails: rarely,
    };
    let mut body = era.otlp(base);
    body["resourceSpans"][0]["resource"]["attributes"]
        .as_array_mut()
        .expect("resource attributes")
        .push(attribute("deployment.environment", "prod"));
    let (status, reply) = server.json("POST", "/v1/traces", Some(&body), &[]);
    assert_eq!(status, 200, "the copies are additive: {reply}");
    let (status, trace) = server.json("GET", &format!("/v1/traces/{:032x}", 1), None, &[]);
    assert_eq!(status, 200, "{trace}");
    let attributes = trace["spans"][0]["attributes"]
        .as_object()
        .expect("attributes");
    assert_eq!(attributes.len(), 9, "four of its own and five copied");
    // The cap still holds for what the span itself carries.
    body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"]
        .as_array_mut()
        .expect("span attributes")
        .push(attribute("one.too.many", "x"));
    let (status, reply) = server.json("POST", "/v1/traces", Some(&body), &[]);
    assert_eq!(status, 400, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or_default()
            .contains("5 attributes exceeds the max-span-attributes cap of 4"),
        "{reply}"
    );
}

#[test]
fn an_analysis_past_the_query_deadline_is_refused_and_counted() {
    let server = Server::spawn(&["--query-deadline-ms", "1"]);
    let base = now_ns() - 2 * DAY;
    // Enough runs that reading them cannot finish inside a millisecond.
    let prompt = "A prompt long enough to parse. ".repeat(20);
    let era = Era {
        tenant: None,
        first: 0,
        last: 2_000,
        version: "1.0.0",
        prompt: &prompt,
        fails: rarely,
    };
    let (status, body) = server.json("POST", "/v1/traces", Some(&era.otlp(base)), &[]);
    assert_eq!(status, 200, "{body}");
    let (status, body) = server.json("GET", &format!("/v1/changes?{}", window(base)), None, &[]);
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("deadline"),
        "{body}"
    );
    let (_, _, metrics) = server.request("GET", "/v1/metrics", None, &[]);
    let counted = metrics
        .lines()
        .find(|line| line.starts_with("traza_query_deadline_exceeded_total "))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok());
    assert!(counted.is_some_and(|count| count >= 1), "{metrics}");
}

#[test]
fn what_changed_answers_an_agent_over_mcp() {
    let server = Server::spawn(&["--mcp"]);
    let base = now_ns() - 2 * DAY;
    ingest_regression(&server, None, base, None);
    let accept = [("Accept", "application/json, text/event-stream")];
    let (status, listed) = server.json(
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
        &accept,
    );
    assert_eq!(status, 200, "{listed}");
    let tool = listed["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "what_changed"))
        .expect("advertised");
    assert_eq!(tool["annotations"]["readOnlyHint"], json!(true));

    let (status, called) = server.json(
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                     "params": {"name": "what_changed",
                                "arguments": {"since": "3d", "agent": "support-bot"}}})),
        &accept,
    );
    assert_eq!(status, 200, "{called}");
    let result = &called["result"];
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(
        text.contains("<traza:telemetry untrusted=\"true\">"),
        "{text}"
    );
    assert!(text.contains("REGRESSION"), "{text}");
    assert!(text.contains("error rate 5.0% → 40.0%"), "{text}");
    let agent = &result["structuredContent"]["agents"][0];
    assert_eq!(agent["agent"], "support-bot");
    assert_eq!(agent["changes"][0]["verdict"], "regression");
}
