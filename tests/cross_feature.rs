//! v0.28's features working on each other's output, through the real
//! binary: a Claude Code history recorded by the importer, with no other
//! setup, answers the Fate Map, the context X-Ray and Cache Autopsy (priced
//! with the pricing file's cache rates), a TQL detector, Agent Blame, the
//! change radar and run topology; and the live view carries each running
//! run's foresight, read from the run guard's memory and the cached Fate
//! Map.
//!
//! The transcripts are the synthetic fixtures in `tests/fixtures/import/`;
//! the live runs are the behaviour corpus ([`traza::seed::behaviour_corpus`]),
//! whose outcomes are exact.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{temp_dir, Server};
use serde_json::{json, Value};
use traza::seed::{behaviour_corpus, BehaviourOptions};

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const OPUS: &str = "llm:claude-opus-5-5";
/// The fixtures are dated January 2026; every windowed read starts before.
const SINCE: &str = "since=1735689600000000000";
/// The vendor's list prices: cache writes at 1.25 times the input rate,
/// cache reads at a twentieth of it for Opus and a tenth for Haiku.
const PRICING: &str = r#"{"models": {
  "claude-opus-5-5": {"input_per_mtok": 4.0, "output_per_mtok": 20.0,
                      "cache_read_per_mtok": 0.2, "cache_write_per_mtok": 5.0},
  "claude-haiku-5-5": {"input_per_mtok": 0.1, "output_per_mtok": 0.5,
                       "cache_read_per_mtok": 0.01, "cache_write_per_mtok": 0.125}}}"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import")
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn ok(server: &Server, path: &str) -> Value {
    let (status, body) = server.json("GET", path, None, &[]);
    assert_eq!(status, 200, "{path}: {body}");
    body
}

fn child<'v>(node: &'v Value, token: &str) -> &'v Value {
    node["children"]
        .as_array()
        .and_then(|children| children.iter().find(|child| child["token"] == token))
        .unwrap_or_else(|| panic!("no child {token} under {node}"))
}

fn close(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|actual| (actual - expected).abs() < 1e-9)
}

/// `traza-server import claude-code` over the fixtures into `server`,
/// hermetic: no inherited `TRAZA_*`, a private HOME, no state file.
fn import_claude_code(server: &Server, home: &Path) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAZA_") {
            command.env_remove(key);
        }
    }
    let path = fixtures().join("claude-code");
    let output = command
        .env("HOME", home)
        .stdin(Stdio::null())
        .args(["import", "claude-code", "--path"])
        .arg(&path)
        .args(["--url", &server.url(""), "--no-state", "--json"])
        .output()
        .expect("runs the importer");
    assert!(
        output.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json is JSON")
}

/// The tape step of `run` at `index`.
fn step(server: &Server, run: &str, index: usize) -> Value {
    let tape = ok(server, &format!("/v1/tapes/{run}"));
    tape["steps"][index].clone()
}

#[test]
fn an_imported_claude_code_history_answers_every_new_read() {
    let home = temp_dir();
    let pricing = home.join("pricing.json");
    std::fs::write(&pricing, PRICING).expect("pricing");
    let server = Server::spawn(&["--pricing", pricing.to_str().expect("utf8")]);
    let imported = import_claude_code(&server, &home);
    assert_eq!(imported["spans_sent"], json!(21), "{imported}");

    // Cost: the pricing file's cache rates price the cache counters the
    // transcript recorded. The first call read 17,920 cached tokens (the
    // system prompt and tool definitions) and wrote 4,180: 3 × $4 + 1,286 ×
    // $20 + 17,920 × $0.20 + 4,180 × $5 per Mtok.
    let first = step(&server, S1, 0);
    assert_eq!(first["kind"], "llm");
    assert!(close(&first["cost_usd"], 0.050_216), "{first}");

    // The Fate Map: both sessions are runs of the claude-code agent, one
    // that succeeded and one that ended on an API error, parting at their
    // first tool.
    let cohorts = ok(&server, &format!("/v1/fate/cohorts?{SINCE}"));
    let cohort = &cohorts["cohorts"][0];
    assert_eq!(
        (
            &cohort["agent"],
            &cohort["runs"],
            &cohort["success"],
            &cohort["failure"]
        ),
        (&json!("claude-code"), &json!(2), &json!(1), &json!(1)),
        "{cohorts}"
    );
    let map = ok(
        &server,
        &format!("/v1/fate/map?agent=claude-code&{SINCE}&min_support=1"),
    );
    let first_call = child(&map["root"], OPUS);
    assert_eq!(first_call["visits"], json!(2));
    let read = child(first_call, "tool:Read");
    let write = child(first_call, "tool:Write");
    assert_eq!((&read["success"], &read["failure"]), (&json!(1), &json!(0)));
    assert_eq!(
        (&write["success"], &write["failure"]),
        (&json!(0), &json!(1))
    );
    assert_eq!(
        map["hotspots"][0]["next"],
        json!(["tool:Read", "tool:Write"])
    );

    // The X-Ray: the importer records each call's conversation delta, and
    // the X-Ray reads the whole context it was — the task, every answer,
    // every tool result — with each chunk's provenance. Step 12 is the
    // main conversation's call after the sub-agent returned.
    let call = step(&server, S1, 12);
    let (trace, span) = (
        call["trace_id"].as_str().expect("trace"),
        call["span_id"].as_str().expect("span"),
    );
    let xray = ok(&server, &format!("/v1/xray/{trace}/{span}"));
    assert_eq!(xray["context_unavailable"], Value::Null, "{xray}");
    let chunks = xray["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 11, "{xray}");
    assert_eq!(chunks[0]["preview"], "Add a total to the cart summary");
    let result = chunks
        .iter()
        .find(|chunk| chunk["preview"] == "pub struct Cart { items: Vec<Item> }")
        .expect("the Read result is in the context");
    assert_eq!(result["kind"], "tool_result");
    assert_eq!(result["introduced_by"]["name"], "Read");
    assert_eq!(result["introduced_by"]["kind"], "tool");
    assert_eq!(xray["previous"]["step_index"], json!(6), "{xray}");
    assert_eq!(xray["dropped_since_previous"], json!([]));
    assert_eq!(xray["cache"]["cause"], "stable");

    // The Cache Autopsy pairs each call with its conversation's previous
    // one — the sub-agent's calls with each other. With a one-minute cache
    // assumed, two calls came back too late; the first of them reports
    // reading its whole prefix from the cache anyway (25,434 tokens, all its
    // predecessor reported), so the cache outlived the assumption and it is
    // stable. The other, an API error, read nothing and reported no counts:
    // its 197 tokens, estimated from the recorded text, are priced at the
    // input rate less the cache-read rate, $3.80 per Mtok.
    let autopsy = ok(&server, &format!("/v1/cache/{S1}?ttl_seconds=60"));
    let verdicts: Vec<(Value, Value, Value)> = autopsy["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| {
            (
                step["step_index"].clone(),
                step["previous_step_index"].clone(),
                step["cause"].clone(),
            )
        })
        .collect();
    assert_eq!(
        verdicts,
        [
            (json!(0), Value::Null, json!("new_context")),
            (json!(2), json!(0), json!("stable")),
            (json!(4), json!(2), json!("stable")),
            (json!(6), json!(4), json!("stable")),
            (json!(9), Value::Null, json!("new_context")),
            (json!(11), json!(9), json!("stable")),
            (json!(12), json!(6), json!("stable")),
            (json!(13), json!(12), json!("ttl_expired")),
            (json!(14), json!(13), json!("stable")),
        ],
        "{autopsy}"
    );
    let summary = &autopsy["summary"];
    assert_eq!(summary["est_tokens_lost"], json!(197));
    assert!(
        close(&summary["est_cost_lost_usd"], 0.000_748_6),
        "{summary}"
    );

    // TQL: the detector library reads the steps the importer wrote — both
    // sessions end on an answer with no test after their last edit — and
    // a query finds the failing test followed by an edit.
    let findings = ok(&server, &format!("/v1/detectors/findings?{SINCE}"));
    let unverified = findings["detectors"]
        .as_array()
        .expect("detectors")
        .iter()
        .find(|detector| detector["name"] == "unverified_finish")
        .expect("listed");
    assert_eq!(unverified["runs_matched"], json!(2), "{unverified}");
    let search = ok(
        &server,
        &format!(
            "/v1/trajectories/search?{SINCE}&q={}",
            encode("test[error] ..> edit")
        ),
    );
    let matched = &search["matches"][0];
    assert_eq!(matched["run"], S1);
    let names: Vec<&Value> = matched["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .map(|span| &span["name"])
        .collect();
    assert_eq!(names, [&json!("Bash"), &json!("MultiEdit")]);

    // Agent Blame: the sub-agent's MultiEdit introduced the line, under the
    // prompt its parent gave it.
    let blame = ok(
        &server,
        &format!(
            "/v1/blame?path={}&line={}",
            encode("src/item.rs"),
            encode("    pub price: u64,")
        ),
    );
    let found = &blame["matches"][0];
    assert_eq!(
        (&found["run"], &found["tool"], &found["agent"]),
        (&json!(S1), &json!("MultiEdit"), &json!("general-purpose")),
        "{blame}"
    );
    assert_eq!(found["prompt"], "Add a price field to Item in src/item.rs");

    // Run topology: the main agent handed work to the sub-agent and back.
    let topology = ok(&server, &format!("/v1/runs/{S1}/topology"));
    assert_eq!(
        topology["sequence"],
        json!(["claude-code", "general-purpose", "claude-code"])
    );
    // The change radar reads the recorder's build attributes: one version,
    // no change across two runs.
    let changes = ok(&server, &format!("/v1/changes?agent=claude-code&{SINCE}"));
    let agent = &changes["agents"][0];
    assert_eq!(agent["runs"], json!(2));
    assert_eq!(agent["current"]["service_version"], "2.1.303");
    assert_eq!(agent["changes"], json!([]));
    // The failed session is known as one everywhere.
    let failed = ok(
        &server,
        &format!("/v1/fate/map?agent=claude-code&{SINCE}&min_support=1"),
    );
    let examples = child(child(&failed["root"], OPUS), "tool:Write")["example_runs"].clone();
    assert_eq!(examples, json!([{"run": S2, "outcome": "failure"}]));
    let _ = std::fs::remove_dir_all(&home);
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// The behaviour corpus split into its completed history and its runs
/// still in flight (`live-*`).
fn corpus() -> (Vec<traza::Span>, Vec<traza::Span>) {
    behaviour_corpus(&BehaviourOptions {
        now_ns: now_ns(),
        live: true,
        ..BehaviourOptions::default()
    })
    .spans
    .into_iter()
    .partition(|span| {
        !span
            .attributes
            .get("session.id")
            .and_then(Value::as_str)
            .is_some_and(|run| run.starts_with("live-"))
    })
}

fn ingest(server: &Server, spans: &[traza::Span]) {
    let body = serde_json::to_value(spans).expect("serializes");
    let (status, reply) = server.json("POST", "/v1/spans", Some(&body), &[]);
    assert_eq!(status, 200, "{reply}");
}

fn live_row<'v>(page: &'v Value, run: &str) -> &'v Value {
    page["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|row| row["run"] == run))
        .unwrap_or_else(|| panic!("{run} is not live: {page}"))
}

#[test]
fn live_runs_carry_their_foresight_from_memory() {
    let server = Server::spawn(&["--mcp"]);
    let (history, live) = corpus();
    ingest(&server, &history);
    ingest(&server, &live);
    // Opening the live view asks for the tenant's Fate Map; the server's
    // background builder makes it, and the view then reads each run's
    // foresight from the run guard's path and the cached map.
    let started = Instant::now();
    let page = loop {
        let page = ok(&server, "/v1/runs/live?limit=6");
        if live_row(&page, "live-doomed-0")["foresight"].is_object()
            && live_row(&page, "live-novel-0")["foresight"].is_object()
        {
            break page;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no foresight on the live view: {page}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    let doomed = &live_row(&page, "live-doomed-0")["foresight"];
    assert_eq!(doomed["verdict"], "likely_failure", "{doomed}");
    assert_eq!(doomed["support"], json!(50));
    assert!(close(&doomed["p_success"], 0.1), "{doomed}");
    assert_eq!(
        live_row(&page, "live-novel-0")["foresight"]["verdict"],
        "novel"
    );
    // The live view and /v1/fate/runs agree.
    let stored = ok(&server, "/v1/fate/runs/live-doomed-0");
    assert_eq!(
        (&stored["verdict"], &stored["support"]),
        (&doomed["verdict"], &doomed["support"])
    );
    // The MCP listing carries it too, as server-computed words and numbers.
    let (status, called) = server.json(
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_live_runs", "arguments": {"limit": 6}}})),
        &[("Accept", "application/json, text/event-stream")],
    );
    assert_eq!(status, 200, "{called}");
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.contains("foresight likely_failure: 10% of 50 runs on its path succeeded"),
        "{text}"
    );
    let rows = called["result"]["structuredContent"]["runs"]
        .as_array()
        .expect("structured rows");
    let structured = rows
        .iter()
        .find(|row| row["run"] == "live-doomed-0")
        .expect("listed");
    assert_eq!(structured["foresight"]["verdict"], "likely_failure");

    // With the Fate Map cache off, no path is kept and no map exists: the
    // live view shows no foresight rather than reading the store for it.
    let off = Server::spawn(&["--fate-cache-seconds", "0"]);
    ingest(&off, &history);
    ingest(&off, &live);
    // Even a run its own pre-call check read whole holds no path then.
    let (status, checked) = off.json(
        "POST",
        "/v1/fuses/check",
        Some(&json!({"run": "live-doomed-0", "next": {"tool": "search_orders"}})),
        &[],
    );
    assert_eq!(status, 200, "{checked}");
    for _ in 0..3 {
        let page = ok(&off, "/v1/runs/live?limit=6");
        assert!(
            page["runs"]
                .as_array()
                .expect("runs")
                .iter()
                .all(|row| row.get("foresight").is_none()),
            "{page}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn live_foresight_is_read_against_the_runs_own_tenant() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:op-token,rw@acme:acme-rw,ro@acme:acme-ro",
        )],
    );
    let op = [("Authorization", "Bearer op-token")];
    let acme_rw = [("Authorization", "Bearer acme-rw")];
    let acme = [("Authorization", "Bearer acme-ro")];
    let send = |spans: &[traza::Span], headers: &[(&str, &str)]| {
        let body = serde_json::to_value(spans).expect("serializes");
        let (status, reply) = server.json("POST", "/v1/spans", Some(&body), headers);
        assert_eq!(status, 200, "{reply}");
    };
    // The default tenant knows the search loop fails; acme has only ever
    // seen clean refunds, and runs the same doomed path live.
    let (history, live) = corpus();
    send(&history, &op);
    send(&live, &op);
    let theirs = behaviour_corpus(&BehaviourOptions {
        now_ns: now_ns(),
        tenant: "acme".to_owned(),
        prefix: "acme-".to_owned(),
        live: true,
        ..BehaviourOptions::default()
    })
    .spans;
    let (acme_history, acme_live): (Vec<traza::Span>, Vec<traza::Span>) =
        theirs.into_iter().partition(|span| {
            !span
                .attributes
                .get("session.id")
                .and_then(Value::as_str)
                .is_some_and(|run| run.starts_with("acme-live-"))
        });
    let refunds: Vec<traza::Span> = acme_history
        .into_iter()
        .filter(|span| {
            span.attributes
                .get("session.id")
                .and_then(Value::as_str)
                .is_some_and(|run| run.starts_with("acme-refund_happy"))
        })
        .collect();
    send(&refunds, &acme_rw);
    send(&acme_live, &acme_rw);

    let foresight = |path: &str, headers: &[(&str, &str)], run: &str| -> Value {
        let started = Instant::now();
        loop {
            let (status, page) = server.json("GET", path, None, headers);
            assert_eq!(status, 200, "{page}");
            let row = live_row(&page, run);
            assert!(
                page["runs"]
                    .as_array()
                    .expect("runs")
                    .iter()
                    .all(|row| row["tenant"] == page["runs"][0]["tenant"]),
                "one tenant per listing: {page}"
            );
            if row["foresight"].is_object() {
                return row["foresight"].clone();
            }
            assert!(started.elapsed() < Duration::from_secs(30), "{page}");
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    // The same path, judged against each tenant's own history.
    let ours = foresight("/v1/runs/live?limit=6&tenant=", &op, "live-doomed-0");
    assert_eq!(ours["verdict"], "likely_failure", "{ours}");
    let mine = foresight("/v1/runs/live?limit=6", &acme, "acme-live-doomed-0");
    assert_eq!(
        mine["verdict"], "novel",
        "acme never took this path: {mine}"
    );
    // A bound credential cannot list another tenant's live runs.
    let (status, refused) = server.json("GET", "/v1/runs/live?tenant=", None, &acme);
    assert_eq!(status, 403, "{refused}");
}
