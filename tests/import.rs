//! Zero-setup recorders and Agent Blame, through the real binary:
//! `traza-server import claude-code|codex` into a running server (over HTTP)
//! and into a data directory, `--watch` following appends and new files and
//! resuming from its state file, and `GET /v1/blame`, `traza-server blame`
//! and the `blame_line` MCP tool finding the step that wrote a line — with
//! tenant isolation for bound credentials throughout.
//!
//! The transcripts are the synthetic fixtures in `tests/fixtures/import/`.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{temp_dir, Server};
use serde_json::{json, Value};

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CART_LINE: &str = "        self.items.iter().map(|item| item.price).sum()";
const PRICING: &str = r#"{"models": {
  "claude-sonnet-4-5*": {"input_per_mtok": 3.0, "output_per_mtok": 15.0},
  "claude-haiku-4-5*": {"input_per_mtok": 1.0, "output_per_mtok": 5.0},
  "gpt-5-codex*": {"input_per_mtok": 1.25, "output_per_mtok": 10.0}}}"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import")
}

/// The binary as a client, hermetic: no inherited `TRAZA_*`, and a private
/// HOME so nothing reads or writes the developer's `~/.traza`.
fn client(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAZA_") {
            command.env_remove(key);
        }
    }
    command.env("HOME", home);
    command.stdin(Stdio::null());
    command
}

fn run(home: &Path, args: &[&str]) -> Output {
    client(home).args(args).output().expect("runs traza-server")
}

fn stats(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "import failed ({:?}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "--json output is JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn import(home: &Path, server: &Server, source: &str, path: &Path, extra: &[&str]) -> Value {
    let url = server.url("");
    let path = path.to_string_lossy().into_owned();
    let mut args = vec!["import", source, "--path", &path, "--url", &url, "--json"];
    args.extend_from_slice(extra);
    stats(&run(home, &args))
}

fn pricing_file(dir: &Path) -> PathBuf {
    let path = dir.join("pricing.json");
    std::fs::write(&path, PRICING).expect("pricing");
    path
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn tape(server: &Server, run: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    server.json("GET", &format!("/v1/tapes/{run}"), None, headers)
}

fn step_kinds(tape: &Value) -> (usize, usize, usize) {
    let steps = tape["steps"].as_array().cloned().unwrap_or_default();
    let count = |kind: &str| steps.iter().filter(|step| step["kind"] == kind).count();
    (count("llm"), count("tool"), count("agent"))
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

fn blame(server: &Server, path: &str, line: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    server.json(
        "GET",
        &format!("/v1/blame?path={}&line={}", encode(path), encode(line)),
        None,
        headers,
    )
}

/// Polls `check` until it holds or `deadline` passes.
fn eventually(what: &str, deadline: Duration, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn an_import_records_runs_steps_tokens_costs_and_outcomes() {
    let home = temp_dir();
    let server = Server::spawn(&["--pricing", pricing_file(&home).to_str().expect("utf8")]);
    let claude = import(
        &home,
        &server,
        "claude-code",
        &fixtures().join("claude-code"),
        &["--no-state"],
    );
    assert_eq!(claude["spans_sent"], json!(21));
    assert_eq!(claude["sessions"], json!(2));
    assert_eq!(
        (
            claude["turns"].clone(),
            claude["llm_steps"].clone(),
            claude["tool_steps"].clone()
        ),
        (json!(3), json!(11), json!(6))
    );
    assert_eq!(claude["input_tokens"], json!(240));
    assert_eq!(claude["output_tokens"], json!(625));
    assert_eq!(claude["cache_read_tokens"], json!(8400));
    // Unknown records and garbage lines are counted, and the import succeeds.
    assert_eq!(
        claude["transcript"]["unknown_records"],
        json!({"x-future-record": 1})
    );
    assert_eq!(claude["transcript"]["malformed_lines"], json!(1));

    // The session is one run: turns, model calls, tool calls, the sub-agent.
    let (status, run) = tape(&server, S1, &[]);
    assert_eq!(status, 200, "{run}");
    assert_eq!(step_kinds(&run), (9, 5, 1));
    let steps = run["steps"].as_array().expect("steps");
    let first = &steps[0];
    assert_eq!(first["kind"], json!("llm"));
    assert_eq!(
        first["tokens"]["input"],
        json!(10),
        "usage counted once per response"
    );
    assert_eq!(first["tokens"]["output"], json!(120));
    assert_eq!(first["tokens"]["cache_read"], json!(1000));
    let cost = first["cost_usd"].as_f64().expect("priced");
    assert!(
        (cost - 0.00183).abs() < 1e-12,
        "10 in x $3 + 120 out x $15 per Mtok: {cost}"
    );
    let tools: Vec<(&str, &str)> = steps
        .iter()
        .filter(|step| step["kind"] == "tool")
        .map(|step| {
            (
                step["tool"].as_str().unwrap_or_default(),
                step["status"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        tools,
        [
            ("Read", "ok"),
            ("Edit", "ok"),
            ("Bash", "error"),
            ("Agent", "ok"),
            ("MultiEdit", "ok")
        ]
    );
    let agent = steps
        .iter()
        .find(|step| step["kind"] == "agent")
        .expect("agent step");
    assert_eq!(agent["agent"], json!("general-purpose"));

    // Sessions roll up the tokens; recall reads outcomes and goals.
    let (_, session) = server.json("GET", &format!("/v1/sessions/{S1}"), None, &[]);
    assert_eq!(session["prompt_tokens"], json!(140), "{session}");
    assert_eq!(session["completion_tokens"], json!(425));
    let (_, recall) = server.json("GET", "/v1/recall?q=cart&outcome=any", None, &[]);
    let recalled = recall["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|r| r["run"] == S1))
        .unwrap_or_else(|| panic!("{recall}"));
    assert_eq!(recalled["outcome"], json!("success"));
    assert_eq!(recalled["goal"], json!("Add a total to the cart summary"));
    let (_, failed) = server.json("GET", "/v1/recall?q=login&outcome=failure", None, &[]);
    assert_eq!(
        failed["runs"][0]["run"],
        json!(S2),
        "the run that ended on an API error"
    );

    // Codex: responses, tool calls, a sub-agent thread inside the root's run.
    let codex = import(
        &home,
        &server,
        "codex",
        &fixtures().join("codex"),
        &["--no-state"],
    );
    assert_eq!(codex["spans_sent"], json!(21));
    let (_, root) = tape(&server, A, &[]);
    assert_eq!(
        step_kinds(&root),
        (9, 4, 1),
        "8 parent responses (2 replayed from items, 1 with unrecorded content) + 1 sub-agent's"
    );
    let (_, stats) = server.json("GET", "/v1/stats/llm?group_by=session", None, &[]);
    let row = stats["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["key"] == A))
        .expect("codex session row");
    assert_eq!(row["prompt_tokens"], json!(5300));
    assert_eq!(row["completion_tokens"], json!(460));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn importing_twice_is_idempotent_with_or_without_a_state_file() {
    let home = temp_dir();
    let server = Server::spawn(&[]);
    let root = fixtures().join("claude-code");
    import(&home, &server, "claude-code", &root, &["--no-state"]);
    let (_, before) = tape(&server, S1, &[]);
    let again = import(&home, &server, "claude-code", &root, &["--no-state"]);
    assert_eq!(again["spans_sent"], json!(21), "sent again …");
    let (_, after) = tape(&server, S1, &[]);
    assert_eq!(
        before, after,
        "… and stored once: derived ids, last write wins"
    );

    let state = home.join("state.json");
    let state = state.to_str().expect("utf8");
    let first = import(&home, &server, "claude-code", &root, &["--state", state]);
    assert_eq!(
        first["spans_sent"],
        json!(21),
        "a new destination key sends all"
    );
    let second = import(&home, &server, "claude-code", &root, &["--state", state]);
    assert_eq!(second["spans_sent"], json!(0));
    assert_eq!(second["files_unchanged"], json!(3));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_import_into_a_data_directory_is_served_like_one_over_http() {
    let home = temp_dir();
    let data = temp_dir();
    let codex = fixtures().join("codex");
    let output = run(
        &home,
        &[
            "import",
            "codex",
            "--path",
            codex.to_str().expect("utf8"),
            "--data-dir",
            data.to_str().expect("utf8"),
            "--json",
        ],
    );
    assert_eq!(stats(&output)["spans_sent"], json!(21));
    let server = Server::spawn_in(data, &[]);
    let (status, root) = tape(&server, A, &[]);
    assert_eq!(status, 200, "{root}");
    assert_eq!(step_kinds(&root), (9, 4, 1));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_bad_credential_or_a_bad_command_line_stops_the_import_with_a_reason() {
    let home = temp_dir();
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "rw:right-token")]);
    let root = fixtures().join("claude-code");
    let url = server.url("");
    let refused = run(
        &home,
        &[
            "import",
            "claude-code",
            "--path",
            root.to_str().expect("utf8"),
            "--url",
            &url,
            "--token",
            "wrong-token",
            "--no-state",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("refused the credential"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    for args in [
        vec!["import", "gemini"],
        vec![
            "import",
            "codex",
            "--url",
            "http://x:1",
            "--data-dir",
            "/tmp/x",
        ],
        vec!["import", "codex", "--path", "/definitely/not/here"],
        vec!["import", "codex", "--since", "forever"],
        vec!["import", "codex", "--tenant", "Not Valid"],
    ] {
        let output = run(&home, &args);
        assert!(!output.status.success(), "{args:?} must be refused");
        assert!(!output.stderr.is_empty(), "{args:?} says why");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// Appends one prompt-and-answer turn to a Claude Code transcript.
fn append_turn(path: &Path, session: &str, key: &str, minute: u32) {
    let prompt = json!({"type": "user", "uuid": format!("u-{key}"), "sessionId": session,
        "timestamp": format!("2026-01-05T12:{minute:02}:00.000Z"), "cwd": "/tmp/demo-shop",
        "message": {"role": "user", "content": format!("prompt {key}")}});
    let answer = json!({"type": "assistant", "uuid": format!("a-{key}"),
        "parentUuid": format!("u-{key}"), "sessionId": session,
        "timestamp": format!("2026-01-05T12:{minute:02}:05.000Z"), "cwd": "/tmp/demo-shop",
        "message": {"id": format!("msg-{key}"), "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": format!("answer {key}")}],
            "usage": {"input_tokens": 1, "output_tokens": 2}}});
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    text.push_str(&format!("{prompt}\n{answer}\n"));
    std::fs::write(path, text).expect("append");
}

fn decoded_spans(server: &Server) -> u64 {
    let (_, _, text) = server.request("GET", "/v1/metrics", None, &[]);
    text.lines()
        .find_map(|line| line.strip_prefix("traza_http_decoded_spans_total "))
        .and_then(|value| value.trim().parse().ok())
        .expect("metric")
}

struct Watcher(Child);

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn watch(home: &Path, server: &Server, root: &Path, state: &Path) -> Watcher {
    let url = server.url("");
    Watcher(
        client(home)
            .args([
                "import",
                "claude-code",
                "--watch",
                "--interval-ms",
                "100",
                "--path",
                root.to_str().expect("utf8"),
                "--url",
                &url,
                "--state",
                state.to_str().expect("utf8"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawns the watcher"),
    )
}

#[test]
fn watch_follows_appends_and_new_files_and_resumes_from_its_state() {
    let home = temp_dir();
    let root = home.join("projects");
    copy_tree(&fixtures().join("claude-code"), &root);
    let state = home.join("state.json");
    let server = Server::spawn(&[]);
    let steps = |run: &str| -> usize {
        let (status, body) = tape(&server, run, &[]);
        if status == 200 {
            body["steps"].as_array().map_or(0, Vec::len)
        } else {
            0
        }
    };
    let watcher = watch(&home, &server, &root, &state);
    eventually("the initial import", Duration::from_secs(30), || {
        steps(S1) == 15
    });

    // An appended turn arrives while the watcher runs.
    let main = root.join("-tmp-demo-shop").join(format!("{S1}.jsonl"));
    append_turn(&main, S1, "live", 1);
    eventually("the appended turn", Duration::from_secs(20), || {
        steps(S1) == 16
    });

    // So does a session that did not exist when it started.
    let s3 = "33333333-3333-4333-8333-333333333333";
    append_turn(
        &root.join("-tmp-demo-shop").join(format!("{s3}.jsonl")),
        s3,
        "new",
        2,
    );
    eventually("the new session", Duration::from_secs(20), || {
        steps(s3) == 1
    });

    // Stopped, the transcript grows; restarted, it sends what is new — and
    // only that: the resend floor comes from the state file.
    drop(watcher);
    let before = decoded_spans(&server);
    append_turn(&main, S1, "offline", 3);
    let _restarted = watch(&home, &server, &root, &state);
    eventually(
        "the turn written while stopped",
        Duration::from_secs(30),
        || steps(S1) == 17,
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        decoded_spans(&server) - before,
        2,
        "the new turn span and its model call, nothing re-sent"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn blame_finds_the_step_that_introduced_a_line_over_http_cli_and_mcp() {
    let home = temp_dir();
    let server = Server::spawn(&["--mcp"]);
    import(
        &home,
        &server,
        "claude-code",
        &fixtures().join("claude-code"),
        &["--no-state"],
    );
    import(
        &home,
        &server,
        "codex",
        &fixtures().join("codex"),
        &["--no-state"],
    );

    let (status, found) = blame(&server, "src/cart.rs", CART_LINE, &[]);
    assert_eq!(status, 200, "{found}");
    let first = &found["matches"][0];
    assert_eq!(first["run"], json!(S1));
    assert_eq!(first["tool"], json!("Edit"));
    assert_eq!(first["file"], json!("/tmp/demo-shop/src/cart.rs"));
    assert_eq!(first["prompt"], json!("Add a total to the cart summary"));
    assert_eq!(first["model"], json!("claude-sonnet-4-5-20250929"));
    assert_eq!(first["step_index"], json!(3));
    assert_eq!(first["service"], json!("claude-code"));
    assert_eq!(found["matches"].as_array().map(Vec::len), Some(1));

    // A sub-agent's edit names the sub-agent and the task it was given.
    let (_, item) = blame(&server, "src/item.rs", "pub price: u64,", &[]);
    assert_eq!(item["matches"][0]["agent"], json!("general-purpose"));
    assert_eq!(
        item["matches"][0]["prompt"],
        json!("Add a price field to Item in src/item.rs")
    );
    // A Write's content and a Codex patch's added lines count too.
    let (_, write) = blame(
        &server,
        "tests/login_test.rs",
        "assert_eq!(session.user_id, 42);",
        &[],
    );
    assert_eq!(write["matches"][0]["tool"], json!("Write"));
    let (_, patch) = blame(&server, "src/app.py", "def health():", &[]);
    assert_eq!(patch["matches"][0]["tool"], json!("apply_patch"));
    assert_eq!(
        patch["matches"][0]["file"],
        json!("/tmp/demo-api/src/app.py")
    );
    assert_eq!(
        patch["matches"][0]["prompt"],
        json!("Add a /health endpoint")
    );
    // A file change recorded only as an item counts too.
    let (_, item) = blame(
        &server,
        "docs/API.md",
        "GET /health returns {\"status\": \"ok\"}",
        &[],
    );
    assert_eq!(item["matches"][0]["tool"], json!("apply_patch"));
    assert_eq!(item["matches"][0]["prompt"], json!("Document the API"));
    // A context line no edit added is nobody's.
    let (_, kept) = blame(&server, "src/app.py", "router = Router()", &[]);
    assert_eq!(kept["matches"], json!([]));

    // Refusals name the problem.
    for (query, needle) in [
        ("/v1/blame?path=src/cart.rs", "path and line are required"),
        ("/v1/blame?path=src/cart.rs&line=%20%20", "line is empty"),
        ("/v1/blame?path=a&line=b&bogus=1", "unknown query parameter"),
        ("/v1/blame?path=a&line=b&limit=0", "limit"),
    ] {
        let (status, body) = server.json("GET", query, None, &[]);
        assert_eq!(status, 400, "{query}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains(needle)),
            "{query}: {body}"
        );
    }

    // The CLI reads the line from the file, relative to its repository.
    let repo = home.join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repo");
    std::fs::create_dir_all(repo.join("src")).expect("src");
    std::fs::write(
        repo.join("src/cart.rs"),
        format!(
            "// Hand-written header.\nimpl Cart {{\n    pub fn total(&self) -> u64 {{\n{CART_LINE}\n    }}\n}}\n"
        ),
    )
    .expect("file");
    let target = format!("{}:4", repo.join("src/cart.rs").display());
    let url = server.url("");
    let cli = run(&home, &["blame", &target, "--url", &url, "--json"]);
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let answer: Value = serde_json::from_slice(&cli.stdout).expect("json");
    assert_eq!(
        answer["path"],
        json!("src/cart.rs"),
        "sent relative to the repository"
    );
    assert_eq!(answer["matches"][0]["span_id"], first["span_id"]);
    let human = run(&home, &["blame", &target, "--url", &url]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("introduced") && text.contains("by Edit"),
        "{text}"
    );
    assert!(
        text.contains("prompt: Add a total to the cart summary"),
        "{text}"
    );
    let nobody = run(
        &home,
        &[
            "blame",
            &format!("{}:1", repo.join("src/cart.rs").display()),
            "--url",
            &url,
        ],
    );
    assert_eq!(nobody.status.code(), Some(1), "no agent wrote the header");

    // The MCP tool, framed as untrusted telemetry.
    let (status, reply) = server.json(
        "POST",
        "/v1/mcp",
        Some(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "blame_line",
            "arguments": {"path": "src/cart.rs", "line": CART_LINE}}}),
        ),
        &[("Accept", "application/json, text/event-stream")],
    );
    assert_eq!(status, 200, "{reply}");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.contains("Edit wrote /tmp/demo-shop/src/cart.rs"),
        "{text}"
    );
    assert!(
        text.contains("prompt: Add a total to the cart summary"),
        "{text}"
    );
    let open = text.find("<traza:telemetry").expect("opened block");
    let row = text.find("Edit wrote").expect("row");
    let close = text.find("</traza:telemetry>").expect("closed block");
    assert!(
        open < row && row < close,
        "stored text stays inside the block"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bound_credentials_import_into_and_blame_only_their_tenant() {
    let home = temp_dir();
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:root-token,rw@acme:acme-token,rw@beta:beta-token",
        )],
    );
    import(
        &home,
        &server,
        "claude-code",
        &fixtures().join("claude-code"),
        &["--no-state", "--token", "acme-token"],
    );
    import(
        &home,
        &server,
        "codex",
        &fixtures().join("codex"),
        &["--no-state", "--token", "beta-token"],
    );
    let acme = [("Authorization", "Bearer acme-token")];
    let beta = [("Authorization", "Bearer beta-token")];
    let root = [("Authorization", "Bearer root-token")];

    let (_, mine) = blame(&server, "src/cart.rs", CART_LINE, &acme);
    assert_eq!(mine["matches"][0]["tenant"], json!("acme"));
    let (_, theirs) = blame(&server, "src/app.py", "def health():", &acme);
    assert_eq!(
        theirs["matches"],
        json!([]),
        "beta's edits are invisible to acme"
    );
    let (_, beta_view) = blame(&server, "src/cart.rs", CART_LINE, &beta);
    assert_eq!(beta_view["matches"], json!([]));
    let (status, _) = server.json(
        "GET",
        &format!(
            "/v1/blame?path=src/cart.rs&line={}&tenant=acme",
            encode(CART_LINE)
        ),
        None,
        &beta,
    );
    assert_eq!(status, 403, "a bound credential cannot name another tenant");
    let (status, _) = tape(&server, S1, &beta);
    assert_eq!(status, 404, "acme's run does not exist for beta");
    let (_, operator) = server.json(
        "GET",
        &format!(
            "/v1/blame?path=src/cart.rs&line={}&tenant=acme",
            encode(CART_LINE)
        ),
        None,
        &root,
    );
    assert_eq!(operator["matches"][0]["run"], json!(S1));
    let _ = std::fs::remove_dir_all(&home);
}
