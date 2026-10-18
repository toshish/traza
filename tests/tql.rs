//! TQL against the real binary: `GET /v1/trajectories/search`,
//! `/v1/trajectories/explain`, `/v1/detectors`, `/v1/detectors/findings`
//! and the MCP `find_behavior` tool — behaviour, refusals with columns,
//! bounds, and tenant isolation.

mod common;

use common::Server;
use serde_json::{json, Value};

/// 2026-03-01T00:00:00Z: every seeded run is long finished.
const T0: u64 = 1_772_323_200_000_000_000;
const SECOND: u64 = 1_000_000_000;

/// Percent-encodes everything but unreserved characters — what a client
/// must do with a TQL query, whose `+` would otherwise arrive as a space.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

struct Run {
    tenant: &'static str,
    session: &'static str,
    /// Offset of the run's first step from T0, in seconds.
    base: u64,
    steps: usize,
}

impl Run {
    fn span(&mut self, status: &str, attributes: Value) -> Value {
        let mut attributes = attributes;
        attributes["session.id"] = json!(self.session);
        let start = T0 + (self.base + self.steps as u64) * SECOND;
        let mut span = json!({
            "trace_id": format!("tr-{}", self.session),
            "span_id": format!("{}-{}", self.session, self.steps),
            "name": "step",
            "service": "coder",
            "status": status,
            "start_time_ns": start,
            "end_time_ns": start + SECOND / 2,
            "attributes": attributes,
        });
        if !self.tenant.is_empty() {
            span["$tenant"] = json!(self.tenant);
        }
        self.steps += 1;
        span
    }

    fn llm(&mut self, catalog: &[&str]) -> Value {
        let tools: Vec<Value> = catalog.iter().map(|name| json!({"name": name})).collect();
        self.span(
            "ok",
            json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "claude-sonnet",
                   "gen_ai.tool.definitions": tools}),
        )
    }

    fn bash(&mut self, command: &str, status: &str) -> Value {
        self.span(
            status,
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Bash",
                   "gen_ai.tool.call.arguments": {"command": command}}),
        )
    }

    fn edit(&mut self, file: &str) -> Value {
        self.span(
            "ok",
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Edit",
                   "gen_ai.tool.call.arguments": {"file_path": file, "old_string": "a",
                                                   "new_string": "b"}}),
        )
    }

    fn tool(&mut self, name: &str) -> Value {
        self.span(
            "ok",
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name}),
        )
    }
}

fn run(tenant: &'static str, session: &'static str, base: u64) -> Run {
    Run {
        tenant,
        session,
        base,
        steps: 0,
    }
}

/// Five runs in the default tenant, one in `acme`, oldest first.
fn seed(server: &Server, headers: &[(&str, &str)]) {
    let catalog = ["Bash", "Edit"];
    let mut spans = Vec::new();

    let mut tampered = run("", "tampered", 0);
    spans.push(tampered.llm(&catalog));
    spans.push(tampered.bash("pytest -q", "error"));
    spans.push(tampered.llm(&catalog));
    spans.push(tampered.edit("tests/test_login.py"));
    spans.push(tampered.bash("pytest -q", "ok"));
    spans.push(tampered.llm(&catalog));

    let mut honest = run("", "honest", 100);
    spans.push(honest.llm(&catalog));
    spans.push(honest.bash("pytest -q", "error"));
    spans.push(honest.edit("src/login.py"));
    spans.push(honest.bash("pytest -q", "ok"));
    spans.push(honest.llm(&catalog));

    let mut thrash = run("", "thrash", 200);
    for _ in 0..5 {
        spans.push(thrash.llm(&catalog));
        spans.push(thrash.edit("src/app.py"));
    }
    spans.push(thrash.bash("make", "error"));

    let mut hallucinated = run("", "hallucinated", 300);
    spans.push(hallucinated.llm(&["Bash"]));
    spans.push(hallucinated.tool("Deploy"));
    spans.push(hallucinated.llm(&["Bash"]));

    let mut acme = run("acme", "acme-tampered", 400);
    spans.push(acme.bash("cargo test", "error"));
    spans.push(acme.edit("tests/integration.rs"));
    spans.push(acme.bash("cargo test", "ok"));
    spans.push(acme.llm(&catalog));

    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), headers);
    assert_eq!(status, 200, "{body}");
}

fn search(server: &Server, query: &str, extra: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let path = format!(
        "/v1/trajectories/search?q={}&since={}{extra}",
        encode(query),
        T0 - 1
    );
    server.json("GET", &path, None, headers)
}

fn runs(body: &Value) -> Vec<String> {
    body["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("no matches in {body}"))
        .iter()
        .map(|found| found["run"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn search_finds_the_shape_and_names_the_steps() {
    let server = Server::spawn(&[]);
    seed(&server, &[]);

    let (status, body) = search(
        &server,
        r#"test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]"#,
        "",
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        runs(&body),
        ["acme-tampered", "tampered"],
        "operator view, newest first"
    );
    let found = &body["matches"][1];
    assert_eq!(found["outcome"], json!("success"));
    assert_eq!(found["agent"], json!("coder"));
    let steps: Vec<(u64, &str)> = found["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .map(|span| {
            (
                span["step_index"].as_u64().unwrap(),
                span["span_id"].as_str().unwrap(),
            )
        })
        .collect();
    // The llm call between the failure and the edit is skipped by `..>` and
    // not listed.
    assert_eq!(
        steps,
        [(1, "tampered-1"), (3, "tampered-3"), (4, "tampered-4")]
    );
    assert_eq!(found["spans"][0]["trace_id"], json!("tr-tampered"));
    assert_eq!(found["spans"][1]["name"], json!("Edit"));
    assert_eq!(body["runs_scanned"], json!(5));
    assert_eq!(body["truncated"], json!(false));
    assert_eq!(body["stopped"], Value::Null);

    // Captures: the same file, five times within ten steps.
    let (status, body) = search(
        &server,
        "edit[file=@f] (..> edit[file=@f]){4,} within 10",
        "&tenant=",
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(runs(&body), ["thrash"]);
    assert_eq!(body["matches"][0]["captures"], json!({"f": "src/app.py"}));
    assert_eq!(body["matches"][0]["outcome"], json!("failure"));
    assert_eq!(
        body["matches"][0]["spans"].as_array().map(Vec::len),
        Some(5)
    );

    // A tool outside its model call's catalog.
    let (_, body) = search(&server, "tool[in_catalog=false]", "&tenant=", &[]);
    assert_eq!(runs(&body), ["hallucinated"]);
    assert_eq!(body["matches"][0]["spans"][0]["name"], json!("Deploy"));
}

#[test]
fn filters_limits_and_a_single_run() {
    let server = Server::spawn(&[]);
    seed(&server, &[]);

    // Only failed runs.
    let (_, body) = search(&server, "edit", "&tenant=&outcome=failure", &[]);
    assert_eq!(runs(&body), ["thrash"]);
    assert_eq!(body["matches"][0]["count"], json!(5));

    // The limit stops the scan at the most recent match and says so.
    let (_, body) = search(&server, "step", "&tenant=&limit=1", &[]);
    assert_eq!(runs(&body), ["hallucinated"]);
    assert_eq!(body["truncated"], json!(true));
    assert_eq!(body["stopped"], json!("limit"));

    // An agent that ran nothing.
    let (_, body) = search(&server, "step", "&tenant=&agent=nobody", &[]);
    assert!(runs(&body).is_empty());
    assert_eq!(body["runs_scanned"], json!(4));

    // A window that ends before the newest runs.
    let until = T0 + 250 * SECOND;
    let (_, body) = search(&server, "step", &format!("&tenant=&until={until}"), &[]);
    assert_eq!(runs(&body), ["thrash", "honest", "tampered"]);

    // One run: every occurrence is listed.
    let (_, body) = search(&server, "test", "&tenant=&run=tampered", &[]);
    assert_eq!(runs(&body), ["tampered"]);
    let occurrences = body["matches"][0]["occurrences"]
        .as_array()
        .expect("listed");
    assert_eq!(occurrences.len(), 2);
    assert_eq!(occurrences[1]["first_step"], json!(4));

    // `+` must be percent-encoded: unencoded it arrives as a space, and the
    // echoed `query` shows the pattern as it was read.
    let path = format!("/v1/trajectories/search?q=tool(Edit)+&since={}", T0 - 1);
    let (_, body) = server.json("GET", &path, None, &[]);
    assert_eq!(body["query"], json!("tool(Edit)"));
    let (_, body) = search(&server, "tool(Edit)+", "", &[]);
    assert_eq!(body["query"], json!("tool(Edit)+"));
}

#[test]
fn refusals_name_the_column_and_the_parameter() {
    let server = Server::spawn(&[]);

    let (status, body) = search(&server, "tool(bash)[stauts=error]", "", &[]);
    assert_eq!(status, 400);
    assert_eq!(body["column"], json!(12));
    let message = body["error"].as_str().expect("error");
    assert!(message.starts_with("column 12:"), "{message}");
    assert!(message.contains("did you mean `status`"), "{message}");

    let (status, body) = search(&server, "tool(a) ..", "", &[]);
    assert_eq!(status, 400);
    assert_eq!(body["column"], json!(11));

    for (query, needle) in [
        ("", "q is required"),
        ("q=step&outcome=sometimes", "outcome must be"),
        ("q=step&limit=0", "limit must be"),
        ("q=step&since=yesterday", "since must be"),
        ("q=step&bogus=1", "unknown query parameter"),
    ] {
        let (status, body) = server.json(
            "GET",
            &format!("/v1/trajectories/search?{query}"),
            None,
            &[],
        );
        assert_eq!(status, 400, "{query}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(needle),
            "{query}: {body}"
        );
    }
    let (status, body) = server.json("GET", "/v1/detectors/findings?detector=nope", None, &[]);
    assert_eq!(status, 400);
    assert!(
        body["error"].as_str().unwrap().contains("unknown detector"),
        "{body}"
    );
    let (status, _) = server.json("POST", "/v1/detectors", Some(&json!({})), &[]);
    assert_eq!(status, 405);
}

#[test]
fn explain_shows_how_a_query_was_read() {
    let server = Server::spawn(&[]);
    let path = format!(
        "/v1/trajectories/explain?q={}",
        encode(
            "tool(a) -> tool(b) | llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6"
        )
    );
    let (status, body) = server.json("GET", &path, None, &[]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["query"],
        json!("tool(a) -> tool(b) | llm[context>=1000 and context=@c] ..> llm[context>=2*@c] within 6")
    );
    assert_eq!(body["captures"], json!(["c"]));
    assert_eq!(body["fields"], json!(["context"]));
    assert_eq!(body["min_steps"], json!(2));
    assert_eq!(body["within"], json!({"steps": 6}));
}

#[test]
fn the_detector_library_and_its_findings() {
    let server = Server::spawn(&[]);
    seed(&server, &[]);

    let (status, library) = server.json("GET", "/v1/detectors", None, &[]);
    assert_eq!(status, 200);
    let names: Vec<&str> = library["detectors"]
        .as_array()
        .expect("detectors")
        .iter()
        .map(|detector| detector["name"].as_str().unwrap())
        .collect();
    for wanted in [
        "test_tampering",
        "give_up_loop",
        "hallucinated_tool",
        "edit_thrash",
        "ping_pong",
        "retry_without_change",
        "context_blowup",
    ] {
        assert!(names.contains(&wanted), "{wanted} missing from {names:?}");
    }
    // Every detector's query is an ordinary TQL query.
    for detector in library["detectors"].as_array().unwrap() {
        let path = format!(
            "/v1/trajectories/explain?q={}",
            encode(detector["query"].as_str().unwrap())
        );
        let (status, body) = server.json("GET", &path, None, &[]);
        assert_eq!(status, 200, "{detector}: {body}");
    }

    let (status, findings) = server.json(
        "GET",
        &format!("/v1/detectors/findings?since={}&tenant=", T0 - 1),
        None,
        &[],
    );
    assert_eq!(status, 200, "{findings}");
    assert_eq!(findings["runs_scanned"], json!(4));
    assert_eq!(findings["baseline"]["success"], json!(3));
    assert_eq!(findings["baseline"]["failure"], json!(1));
    let row = |name: &str| {
        findings["detectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    };
    let tampering = row("test_tampering");
    assert_eq!(tampering["runs_matched"], json!(1));
    assert_eq!(tampering["examples"][0]["run"], json!("tampered"));
    assert_eq!(tampering["success_rate"], json!(1.0));
    assert_eq!(row("edit_thrash")["runs_matched"], json!(1));
    assert_eq!(row("edit_thrash")["outcomes"]["failure"], json!(1));
    assert_eq!(row("hallucinated_tool")["runs_matched"], json!(1));
    assert_eq!(row("ping_pong")["runs_matched"], json!(0));
    // Findings come first, high severity first.
    assert_eq!(findings["detectors"][0]["severity"], json!("high"));
    assert!(findings["detectors"][0]["runs_matched"].as_u64().unwrap() > 0);

    let (_, one) = server.json(
        "GET",
        &format!(
            "/v1/detectors/findings?since={}&detector=edit_thrash",
            T0 - 1
        ),
        None,
        &[],
    );
    assert_eq!(one["detectors"].as_array().map(Vec::len), Some(1));
}

/// About 4,080 regex instructions, all live on every character of a text
/// without a NUL: seconds of work over 64 KiB, even in a release build.
const HEAVY: &str = r#"(?:[^\x00]{255}){16}\x00"#;

#[test]
fn hostile_queries_and_data_stay_bounded() {
    let server = Server::spawn(&[]);
    let mut spans = Vec::new();
    // A model call offering 64 tools: five captures over its catalog are
    // 64^5 combinations.
    let mut wide = run("", "wide", 0);
    let catalog: Vec<String> = (0..64).map(|i| format!("tool_{i}")).collect();
    let catalog: Vec<&str> = catalog.iter().map(String::as_str).collect();
    spans.push(wide.llm(&catalog));
    // A command of 5 KiB, captured.
    let mut long = run("", "long", 100);
    let command = format!("pytest {}", "x".repeat(5_000));
    spans.push(long.bash(&command, "ok"));
    // A failed test, a test-file edit, then a plugin install: no test ever
    // passed, so this is not test tampering.
    let mut install = run("", "install", 200);
    spans.push(install.bash("pytest -q", "error"));
    spans.push(install.edit("tests/test_a.py"));
    spans.push(install.bash("pip install pytest-xdist", "ok"));
    spans.push(install.llm(&["Bash", "Edit"]));
    // A span whose session id claims the `trace:` form with no trace.
    let mut poisoned = run("", "trace:", 300);
    spans.push(poisoned.bash("ls", "ok"));
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &[]);
    assert_eq!(status, 200, "{body}");

    // The fan-out is cut at the path cap and flagged, promptly.
    let started = std::time::Instant::now();
    let (status, body) = search(
        &server,
        "llm[offered=@a, offered=@b, offered=@c, offered=@d, offered=@e]",
        "",
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // `install` offers two tools (32 combinations, all kept); `wide` 64.
    assert_eq!(runs(&body), ["install", "wide"]);
    assert_eq!(body["matches"][0]["inexact"], json!(false), "{body}");
    assert_eq!(body["matches"][1]["inexact"], json!(true), "{body}");

    // The poisoned span is left out and counted; the search answers.
    assert_eq!(body["spans_skipped"], json!(1), "{body}");
    let (status, findings) = server.json(
        "GET",
        &format!("/v1/detectors/findings?since={}", T0 - 1),
        None,
        &[],
    );
    assert_eq!(status, 200, "{findings}");
    assert_eq!(findings["spans_skipped"], json!(1));
    assert_eq!(findings["runs_checked"], json!(3));
    let tampering = findings["detectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == json!("test_tampering"))
        .expect("row");
    assert_eq!(
        tampering["runs_matched"],
        json!(0),
        "installing pytest is not a passing test run"
    );

    // Captured values come back cut to 1 KiB, and say so.
    let (status, body) = search(&server, "tool[args=@a]", "&run=long", &[]);
    assert_eq!(status, 200, "{body}");
    let captured = body["matches"][0]["captures"]["a"].as_str().expect("text");
    assert_eq!(captured.len(), 1024);
    assert_eq!(body["matches"][0]["captures_truncated"], json!(true));

    // A query holds at most 8192 regex instructions, refused at the column
    // of the regex that crosses it.
    let three = format!("tool[args~\"{HEAVY}\" or result~\"{HEAVY}\" or span~\"{HEAVY}\"]");
    let (status, body) = search(&server, &three, "", &[]);
    assert_eq!(status, 400, "{body}");
    let column = three.rfind("~\"").unwrap() + 3;
    assert_eq!(body["column"], json!(column), "{body}");
}

#[test]
fn a_heavy_regex_stops_at_the_query_deadline() {
    let server = Server::spawn(&["--query-deadline-ms", "1000"]);
    let mut heavy = run("", "heavy", 0);
    let spans = vec![heavy.bash(&"a".repeat(64 * 1024), "ok")];
    let (status, body) = server.json("POST", "/v1/spans", Some(&json!(spans)), &[]);
    assert_eq!(status, 200, "{body}");
    let query = format!("tool[args~\"{HEAVY}\" or args~\"{HEAVY}y\"]");
    let started = std::time::Instant::now();
    let (status, body) = search(&server, &query, "", &[]);
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stopped"], json!("deadline"), "{body}");
    assert_eq!(body["truncated"], json!(true));
    assert_eq!(body["runs_scanned"], json!(0));
    assert_eq!(body["next_until_ns"], json!(T0));
    // The two regexes alone take several seconds; the request ends within a
    // sliver of its one-second budget.
    assert!(
        elapsed < std::time::Duration::from_millis(2_500),
        "answered after {elapsed:?}"
    );
}

#[test]
fn a_bound_credential_sees_only_its_tenant() {
    let server = Server::spawn_with_env(
        &["--mcp"],
        &[("TRAZA_TOKENS", "rw:op-token,ro@acme:acme-token")],
    );
    let op = [("Authorization", "Bearer op-token")];
    let acme = [("Authorization", "Bearer acme-token")];
    seed(&server, &op);
    let query = r#"test[error] ..> edit ..> test[not error]"#;

    let (status, body) = search(&server, query, "", &acme);
    assert_eq!(status, 200, "{body}");
    assert_eq!(runs(&body), ["acme-tampered"]);
    assert_eq!(
        body["runs_scanned"],
        json!(1),
        "other tenants' runs are not even read"
    );
    assert_eq!(body["matches"][0]["tenant"], json!("acme"));

    let (status, body) = search(&server, query, "&tenant=", &acme);
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["required"], json!("unbound"));
    let (status, _) = search(&server, query, "&tenant=acme", &acme);
    assert_eq!(status, 200);

    // `run=` cannot reach another tenant's run either.
    let (status, body) = search(&server, "step", "&run=tampered", &acme);
    assert_eq!(status, 200);
    assert!(runs(&body).is_empty(), "{body}");

    let findings_path = format!("/v1/detectors/findings?since={}", T0 - 1);
    let (status, findings) = server.json("GET", &findings_path, None, &acme);
    assert_eq!(status, 200, "{findings}");
    assert_eq!(findings["runs_scanned"], json!(1));
    let (status, _) = server.json("GET", &format!("{findings_path}&tenant="), None, &acme);
    assert_eq!(status, 403);

    // The unbound operator sees every tenant.
    let (_, body) = search(&server, query, "", &op);
    assert_eq!(runs(&body), ["acme-tampered", "honest", "tampered"]);

    // MCP is a different wire, not a different authority.
    let call = |headers: &[(&str, &str)]| {
        let mut headers = headers.to_vec();
        headers.push(("Accept", "application/json, text/event-stream"));
        let (status, body) = server.json(
            "POST",
            "/v1/mcp",
            Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": "find_behavior",
                                    "arguments": {"detector": "test_tampering",
                                                  "since": "2026-02-28"}}})),
            &headers,
        );
        assert_eq!(status, 200, "{body}");
        body["result"].clone()
    };
    let bound = call(&acme);
    assert_eq!(bound["isError"], json!(false), "{bound}");
    let bound_runs: Vec<&Value> = bound["structuredContent"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|found| &found["run"])
        .collect();
    assert_eq!(bound_runs, [&json!("acme-tampered")]);
    let text = bound["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("<traza:telemetry"), "{text}");
    let operator = call(&op);
    assert_eq!(
        operator["structuredContent"]["matches"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}
