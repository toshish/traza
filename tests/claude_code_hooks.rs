//! The Claude Code hook pack (`examples/claude-code/`) against the real
//! server: each hook event fed the JSON Claude Code pipes to a hook, with
//! the exact JSON it answers and its exit code asserted — the contract in
//! Claude Code's hooks reference, https://code.claude.com/docs/en/hooks:
//!
//! - PreToolUse answers `permissionDecision: "deny"` with the reason on a
//!   `stop` (a fuse, a kill switch, or a person's `block` nudge), and an
//!   advice nudge as `additionalContext` with NO permission decision (an
//!   "allow" would skip the user's own prompt); otherwise nothing.
//! - SessionStart answers `additionalContext` summarizing earlier runs.
//! - PostToolUse records the call when asked to, and answers
//!   `decision: "block"` when that recording stopped the run.
//! - It fails open unless TRAZA_FAIL_CLOSED=1.
//!
//! And the installer: it merges into an existing settings file without
//! touching anything else, twice is once, uninstall restores, a broken file
//! is refused — and the command it writes runs the hook as Claude Code
//! would.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::Server;
use serde_json::{json, Value};

const HOOK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/claude-code/traza_hook.py"
);
const INSTALLER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/claude-code/install.py"
);

/// What one hook run produced.
#[derive(Debug)]
struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Outcome {
    /// The single JSON object a hook must print alone on stdout, or `None`
    /// when it printed nothing.
    fn answer(&self) -> Option<Value> {
        if self.stdout.is_empty() {
            return None;
        }
        assert_eq!(
            self.stdout.matches('\n').count(),
            1,
            "one JSON object, alone on stdout: {self:?}"
        );
        assert!(self.stdout.ends_with('\n'));
        Some(serde_json::from_str(self.stdout.trim()).expect("stdout is JSON"))
    }
}

/// Whether python3 is here to run the pack; says why not when it is not.
fn python_available() -> bool {
    let ok = Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !ok {
        eprintln!("skipping: the hook pack needs python3");
    }
    ok
}

/// A command with every TRAZA_ and CLAUDE_ variable of this process
/// removed — the suite may itself run under Claude Code — plus `env`.
fn hermetic(command: &mut Command, env: &[(&str, &str)]) {
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("TRAZA_") || key.starts_with("CLAUDE_") {
            command.env_remove(key);
        }
    }
    command.envs(env.iter().copied());
}

fn run_with(mut command: Command, input: &[u8], env: &[(&str, &str)]) -> Outcome {
    hermetic(&mut command, env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawns the hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("writes the hook input");
    let output = child.wait_with_output().expect("the hook exits");
    Outcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Runs the hook script with `input` on stdin.
fn hook(input: &Value, env: &[(&str, &str)]) -> Outcome {
    let mut command = Command::new("python3");
    command.arg("-I").arg(HOOK);
    run_with(command, input.to_string().as_bytes(), env)
}

/// The JSON Claude Code pipes to a PreToolUse hook (common fields too).
fn pre_tool_use(session: &str, command: &str) -> Value {
    json!({
        "session_id": session,
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": "/work/shop",
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command, "description": "Run it"},
        "tool_use_id": "toolu_01ABC",
    })
}

fn post_tool_use(session: &str, command: &str, id: &str) -> Value {
    let mut input = pre_tool_use(session, command);
    input["hook_event_name"] = json!("PostToolUse");
    input["tool_use_id"] = json!(id);
    input["tool_response"] = json!({"stdout": "FAILED 3 tests", "stderr": ""});
    input["duration_ms"] = json!(1200);
    input
}

fn deny(reason: &str) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
           "permissionDecision": "deny", "permissionDecisionReason": reason}})
}

fn nudge(server: &Server, run: &str, kind: &str, message: &str) -> Value {
    let (status, body) = server.json(
        "POST",
        &format!("/v1/runs/{run}/nudges"),
        Some(&json!({"message": message, "kind": kind})),
        &[],
    );
    assert_eq!(status, 201, "{body}");
    body
}

#[test]
fn pre_tool_use_relays_nudges_and_denies_on_a_stop() {
    if !python_available() {
        return;
    }
    let server = Server::spawn(&["--fuse-max-repeats", "3", "--fuse-action", "stop"]);
    let url = server.url("");
    let env = [("TRAZA_URL", url.as_str())];

    let fresh = hook(&pre_tool_use("cc-1", "npm test"), &env);
    assert_eq!((fresh.code, fresh.answer()), (0, None), "{fresh:?}");

    // Advice rides along as context, and decides nothing.
    nudge(&server, "cc-1", "advice", "Run one test file at a time.");
    let advised = hook(&pre_tool_use("cc-1", "npm test"), &env);
    assert_eq!(advised.code, 0);
    assert_eq!(
        advised.answer(),
        Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "additionalContext": "Traza relayed a note from a person supervising run cc-1:\n  \u{201c}Run one test file at a time.\u{201d}"}}))
    );
    let again = hook(&pre_tool_use("cc-1", "npm test"), &env);
    assert_eq!(again.answer(), None, "a nudge reaches one call: {again:?}");

    // A block is a veto of the next call, and only of it.
    nudge(&server, "cc-1", "block", "Do not deploy on a Friday.");
    let vetoed = hook(&pre_tool_use("cc-1", "npm run deploy"), &env);
    assert_eq!(vetoed.code, 0);
    assert_eq!(
        vetoed.answer(),
        Some(deny(
            "Traza blocked this tool call: a person supervising run cc-1 vetoed it, writing:\n  \u{201c}Do not deploy on a Friday.\u{201d}"
        ))
    );
    let after = hook(&pre_tool_use("cc-1", "npm test"), &env);
    assert_eq!(after.answer(), None, "the veto did not trip the fuse");
    let (_, run) = server.json("GET", "/v1/fuses/runs/cc-1", None, &[]);
    assert_eq!(
        (run["verdict"].as_str(), &run["tripped_by"]),
        (Some("continue"), &Value::Null)
    );

    // Without TRAZA_RECORD_TOOLS the PostToolUse half records nothing.
    let unrecorded = hook(&post_tool_use("cc-1", "npm test", "toolu_0"), &env);
    assert_eq!((unrecorded.code, unrecorded.answer()), (0, None));
    let (_, spans) = server.json("GET", "/v1/spans?service=claude-code", None, &[]);
    assert_eq!(spans["spans"].as_array().map(Vec::len), Some(0), "{spans}");

    // Recording: the third identical call stops the run, and says so.
    let recording = [("TRAZA_URL", url.as_str()), ("TRAZA_RECORD_TOOLS", "1")];
    for id in ["toolu_1", "toolu_2"] {
        let recorded = hook(&post_tool_use("cc-1", "npm test", id), &recording);
        assert_eq!(
            (recorded.code, recorded.answer()),
            (0, None),
            "{recorded:?}"
        );
    }
    let third = hook(&post_tool_use("cc-1", "npm test", "toolu_3"), &recording);
    let (_, run) = server.json("GET", "/v1/fuses/runs/cc-1", None, &[]);
    let reason = run["reasons"][0]
        .as_str()
        .expect("a stop reason")
        .to_owned();
    assert!(
        reason.starts_with("max_repeats 3 reached: tool:Bash#"),
        "{run}"
    );
    assert_eq!(third.code, 0);
    assert_eq!(
        third.answer(),
        Some(json!({"decision": "block", "reason": format!(
            "Traza stopped run cc-1: {reason}. This tool call already ran; the next one will be refused."
        )}))
    );
    let refused = hook(&pre_tool_use("cc-1", "npm test"), &env);
    assert_eq!(
        refused.answer(),
        Some(deny(&format!(
            "Traza stopped run cc-1: {reason}. Its limits allow no further steps, so this call \
             was not made; the user can reset the run in Traza."
        )))
    );
    // Recorded with a digest of the input, never the command itself.
    let (_, spans) = server.json("GET", "/v1/spans?service=claude-code", None, &[]);
    let rows = spans["spans"].as_array().expect("spans");
    assert_eq!(rows.len(), 3, "{spans}");
    assert!(rows
        .iter()
        .all(|span| span["attributes"]["gen_ai.tool.call.arguments"]
            .as_str()
            .is_some_and(|text| text.starts_with("sha256:"))));
    assert!(!spans.to_string().contains("npm test"));
    assert!(rows.iter().any(|span| span["span_id"] == "toolu_3"));

    // A person's veto and the fuse together: the person speaks first.
    nudge(&server, "cc-1", "block", "Stop and summarize.");
    let both = hook(&pre_tool_use("cc-1", "npm test"), &env);
    let reason_text = both.answer().expect("an answer")["hookSpecificOutput"]
        ["permissionDecisionReason"]
        .as_str()
        .expect("a reason")
        .to_owned();
    assert!(
        reason_text.contains("\u{201c}Stop and summarize.\u{201d}"),
        "{reason_text}"
    );

    // The kill switch, with its reason.
    server.json(
        "POST",
        "/v1/fuses/runs/cc-kill/trip",
        Some(&json!({"reason": "hold for review"})),
        &[],
    );
    let killed = hook(&pre_tool_use("cc-kill", "ls"), &env);
    assert_eq!(
        killed.answer(),
        Some(deny(
            "Traza stopped run cc-kill: manually tripped: hold for review. Its limits allow no \
             further steps, so this call was not made; the user can reset the run in Traza."
        ))
    );
    // TRAZA_RUN names the run when set.
    nudge(&server, "named-run", "advice", "Hello.");
    let named = hook(
        &pre_tool_use("cc-other", "ls"),
        &[("TRAZA_URL", url.as_str()), ("TRAZA_RUN", "named-run")],
    );
    assert!(
        named.stdout.contains("supervising run named-run"),
        "{named:?}"
    );
}

#[test]
fn the_hook_fails_open_unless_told_to_fail_closed() {
    if !python_available() {
        return;
    }
    let down = [("TRAZA_URL", "http://127.0.0.1:1")];
    let open = hook(&pre_tool_use("cc-down", "ls"), &down);
    assert_eq!((open.code, open.answer()), (0, None));
    assert!(open.stderr.contains("letting it through"), "{open:?}");

    let closed = [
        ("TRAZA_URL", "http://127.0.0.1:1"),
        ("TRAZA_FAIL_CLOSED", "1"),
    ];
    let refused = hook(&pre_tool_use("cc-down", "ls"), &closed);
    assert_eq!(refused.code, 0);
    let answer = refused.answer().expect("a denial");
    assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = answer["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("reason");
    assert!(
        reason.starts_with("Traza could not say whether this step may run (Traza did not answer at http://127.0.0.1:1")
            && reason.ends_with("configured to block tool calls when it cannot (TRAZA_FAIL_CLOSED=1)."),
        "{reason}"
    );
    // The guard example's spelling still works.
    let legacy = hook(
        &pre_tool_use("cc-down", "ls"),
        &[
            ("TRAZA_URL", "http://127.0.0.1:1"),
            ("TRAZA_FUSE_FAIL_CLOSED", "1"),
        ],
    );
    assert!(legacy.answer().is_some());

    // Input that is not a hook's: which event it was is unknown, so exit 2,
    // the one blocking answer every event understands — or nothing.
    let mut command = Command::new("python3");
    command.arg("-I").arg(HOOK);
    let garbled = run_with(command, b"not json", &closed);
    assert_eq!(
        (garbled.code, garbled.stdout.as_str()),
        (2, ""),
        "{garbled:?}"
    );
    assert!(garbled.stderr.contains("TRAZA_FAIL_CLOSED=1"));
    let mut command = Command::new("python3");
    command.arg("-I").arg(HOOK);
    let tolerated = run_with(command, b"not json", &down);
    assert_eq!((tolerated.code, tolerated.stdout.as_str()), (0, ""));

    // No session id: nothing to check, so the configured failure mode.
    let mut nameless = pre_tool_use("x", "ls");
    nameless
        .as_object_mut()
        .expect("object")
        .remove("session_id");
    assert_eq!(hook(&nameless, &down).answer(), None);
    assert!(hook(&nameless, &closed).answer().is_some());

    // An error instead of a verdict is no answer either: a bound token
    // naming another tenant is refused with 403.
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "ro@acme:acme-reader")]);
    let url = server.url("");
    let foreign = [
        ("TRAZA_URL", url.as_str()),
        ("TRAZA_TOKEN", "acme-reader"),
        ("TRAZA_TENANT", "other"),
    ];
    let refused = hook(&pre_tool_use("cc-1", "ls"), &foreign);
    assert_eq!(refused.answer(), None);
    assert!(refused.stderr.contains("HTTP 403"), "{refused:?}");
    // Nor do other events ever block, fail-closed or not.
    let start = json!({"session_id": "s", "hook_event_name": "SessionStart",
                       "source": "startup", "cwd": "/work/shop"});
    let quiet = hook(&start, &closed);
    assert_eq!((quiet.code, quiet.answer()), (0, None), "{quiet:?}");
}

/// A server that answers every request with a redirect to `location`,
/// counting the requests it gets.
fn redirector(location: String) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("addr").port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                line.clear();
            }
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (port, hits)
}

#[test]
fn the_hook_never_follows_a_redirect_with_its_token() {
    if !python_available() {
        return;
    }
    let (elsewhere, elsewhere_hits) = redirector("http://127.0.0.1:1/".to_owned());
    let (front, front_hits) = redirector(format!("http://127.0.0.1:{elsewhere}/v1/fuses/check"));
    let url = format!("http://127.0.0.1:{front}");
    let outcome = hook(
        &pre_tool_use("cc-r", "ls"),
        &[("TRAZA_URL", url.as_str()), ("TRAZA_TOKEN", "secret")],
    );
    assert_eq!((outcome.code, outcome.answer()), (0, None), "{outcome:?}");
    assert!(outcome.stderr.contains("HTTP 307"), "{outcome:?}");
    assert_eq!(front_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        elsewhere_hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect was followed, token and all"
    );
}

/// A finished, successful run in session `run` whose spans name the project
/// directory, as the Claude Code importer records it.
fn finished_run(run: &str, project: &str, start: u64) -> Value {
    let trace = format!("{:0>32}", run.len());
    let span = |id: &str, offset: u64, attributes: Value| {
        json!({"trace_id": trace, "span_id": id, "name": id, "service": "claude-code",
               "start_time_ns": start + offset, "end_time_ns": start + offset + 1_000_000,
               "attributes": attributes})
    };
    json!([
        span(
            "turn",
            0,
            json!({"session.id": run, "gen_ai.operation.name": "chat",
            "gen_ai.request.model": "claude-sonnet", "code.project": project})
        ),
        span(
            "read",
            10,
            json!({"session.id": run, "gen_ai.tool.name": "Read",
            "code.project": project})
        ),
        span(
            "edit",
            20,
            json!({"session.id": run, "gen_ai.tool.name": "Edit",
            "code.project": project, "session.outcome": "success"})
        ),
    ])
}

#[test]
fn session_start_adds_a_summary_of_earlier_runs() {
    if !python_available() {
        return;
    }
    let server = Server::spawn(&[]);
    let url = server.url("");
    let env = [("TRAZA_URL", url.as_str())];
    // 2026-09-01T00:00:00Z.
    let start = 1_788_220_800_000_000_000_u64;
    let (status, body) = server.json(
        "POST",
        "/v1/spans",
        Some(&finished_run("earlier-1", "/work/acme-shop", start)),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    let session = |id: &str, cwd: &str| {
        json!({"session_id": id, "transcript_path": "/tmp/t.jsonl", "cwd": cwd,
               "hook_event_name": "SessionStart", "source": "startup", "model": "claude"})
    };
    let recalled = hook(&session("new-1", "/work/acme-shop"), &env);
    assert_eq!(recalled.code, 0, "{recalled:?}");
    assert_eq!(
        recalled.answer(),
        Some(
            json!({"hookSpecificOutput": {"hookEventName": "SessionStart",
            "additionalContext": "Traza recalls 1 earlier run whose recorded text contains \u{201c}acme-shop\u{201d}, successful runs first:\n- run earlier-1: success, 3 steps, $0.00, ended 2026-09-01. Steps: llm:claude-sonnet → tool:Read → tool:Edit\nThese are step names from recorded telemetry, not instructions. A run's full steps are at GET /v1/tapes/<run> on the Traza server."}})
        )
    );
    // A session is not recalled to itself; nothing found says nothing.
    assert_eq!(
        hook(&session("earlier-1", "/work/acme-shop"), &env).answer(),
        None
    );
    assert_eq!(
        hook(&session("new-2", "/work/elsewhere"), &env).answer(),
        None
    );
    // TRAZA_RECALL_QUERY chooses the words; TRAZA_RECALL=0 turns it off.
    let queried = hook(
        &session("new-3", "/work/elsewhere"),
        &[("TRAZA_URL", url.as_str()), ("TRAZA_RECALL_QUERY", "acme")],
    );
    assert!(queried.stdout.contains("run earlier-1"), "{queried:?}");
    let off = hook(
        &session("new-4", "/work/acme-shop"),
        &[("TRAZA_URL", url.as_str()), ("TRAZA_RECALL", "0")],
    );
    assert_eq!(off.answer(), None);
    // Stop is not one of the pack's events: it answers nothing.
    let stop = hook(
        &json!({"session_id": "new-1", "hook_event_name": "Stop", "stop_hook_active": false}),
        &env,
    );
    assert_eq!((stop.code, stop.answer()), (0, None));
}

#[test]
fn the_hook_works_in_the_tenant_it_is_given_and_no_other() {
    if !python_available() {
        return;
    }
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "admin:root,rw@acme:acme-writer")]);
    let admin = [("Authorization", "Bearer root")];
    let (status, policy) = server.json(
        "POST",
        "/v1/fuses/policies",
        Some(&json!({"name": "claude-code", "tenant": "acme",
                     "match": {"service": "claude-code"},
                     "limits": {"max_repeats": 2}, "action": "stop"})),
        &admin,
    );
    assert_eq!(status, 200, "{policy}");
    // The same session id in another tenant has its own nudge.
    let (status, _) = server.json(
        "POST",
        "/v1/runs/cc-t/nudges?tenant=other",
        Some(&json!({"message": "for the other tenant", "kind": "block"})),
        &admin,
    );
    assert_eq!(status, 201);
    let url = server.url("");
    // The bound token needs no TRAZA_TENANT: it is always its own tenant.
    let env = [
        ("TRAZA_URL", url.as_str()),
        ("TRAZA_TOKEN", "acme-writer"),
        ("TRAZA_RECORD_TOOLS", "1"),
    ];
    let first = hook(&pre_tool_use("cc-t", "make"), &env);
    assert_eq!(
        first.answer(),
        None,
        "the other tenant's veto is not acme's: {first:?}"
    );
    hook(&post_tool_use("cc-t", "make", "toolu_a"), &env);
    let stopped = hook(&post_tool_use("cc-t", "make", "toolu_b"), &env);
    assert_eq!(stopped.answer().expect("a block")["decision"], "block");
    let refused = hook(&pre_tool_use("cc-t", "make"), &env);
    assert_eq!(
        refused.answer().expect("a denial")["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    let (_, acme) = server.json("GET", "/v1/fuses/runs/cc-t?tenant=acme", None, &admin);
    assert_eq!(
        (
            acme["policy"].as_str(),
            acme["usage"]["tool_calls"].as_u64()
        ),
        (Some("claude-code"), Some(2)),
        "{acme}"
    );
    let (_, default) = server.json("GET", "/v1/fuses/runs/cc-t", None, &admin);
    assert_eq!(default["usage"]["steps"], 0, "nothing leaked: {default}");
    let (_, other) = server.json("GET", "/v1/runs/cc-t/nudges?tenant=other", None, &admin);
    assert_eq!(other["nudges"][0]["status"], "pending", "{other}");
}

fn fresh_project(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-cc-install-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join(".claude")).expect("project dir");
    dir
}

fn install(project: &Path, args: &[&str]) -> Outcome {
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg(INSTALLER)
        .arg("--project")
        .arg(project)
        .args(args);
    run_with(command, b"", &[])
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("reads")).expect("JSON")
}

#[test]
fn the_installer_merges_once_and_uninstalls_to_what_was_there() {
    if !python_available() {
        return;
    }
    let project = fresh_project("merge");
    let settings = project.join(".claude/settings.json");
    let original = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(npm test)"]},
        "hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "./lint.sh"}]}
        ]},
    });
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&original).expect("encodes"),
    )
    .expect("writes settings");

    let installed = install(
        &project,
        &["--url", "http://traza.internal:8080", "--tenant", "acme"],
    );
    assert_eq!(installed.code, 0, "{installed:?}");
    let command = "python3 \"${CLAUDE_PROJECT_DIR}/.claude/hooks/traza/traza_hook.py\"";
    let ours = json!({"type": "command", "command": command, "timeout": 10});
    let expected = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(npm test)"]},
        "hooks": {
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "./lint.sh"}]},
                {"matcher": "*", "hooks": [ours.clone()]},
            ],
            "SessionStart": [{"hooks": [ours.clone()]}],
        },
    });
    assert_eq!(read_json(&settings), expected);
    assert_eq!(
        read_json(&project.join(".claude/hooks/traza/config.json")),
        json!({"url": "http://traza.internal:8080", "tenant": "acme"})
    );
    assert_eq!(
        std::fs::read(project.join(".claude/hooks/traza/traza_hook.py")).expect("copied"),
        std::fs::read(HOOK).expect("source")
    );
    assert_eq!(
        read_json(&project.join(".claude/settings.json.traza-backup")),
        original
    );

    // Twice is once.
    let first = std::fs::read_to_string(&settings).expect("reads");
    assert_eq!(
        install(
            &project,
            &["--url", "http://traza.internal:8080", "--tenant", "acme"]
        )
        .code,
        0
    );
    assert_eq!(std::fs::read_to_string(&settings).expect("reads"), first);

    // Options change the entries, never duplicate them.
    assert_eq!(
        install(&project, &["--record-tools", "--no-recall"]).code,
        0
    );
    let recorded = read_json(&settings);
    let hooks = recorded["hooks"].as_object().expect("hooks");
    let mut events: Vec<&str> = hooks.keys().map(String::as_str).collect();
    events.sort_unstable();
    assert_eq!(
        events,
        ["PostToolUse", "PostToolUseFailure", "PreToolUse"],
        "{recorded}"
    );
    assert_eq!(hooks["PreToolUse"].as_array().map(Vec::len), Some(2));

    // A dry run writes nothing.
    let before = std::fs::read_to_string(&settings).expect("reads");
    let dry = install(&project, &["--dry-run"]);
    assert_eq!(dry.code, 0);
    assert!(dry.stdout.contains("SessionStart"), "{dry:?}");
    assert_eq!(std::fs::read_to_string(&settings).expect("reads"), before);

    // Uninstall restores the original structure and removes the pack.
    assert_eq!(install(&project, &["--uninstall"]).code, 0);
    assert_eq!(read_json(&settings), original);
    assert!(!project.join(".claude/hooks/traza").exists());

    // A broken settings file is refused and left as it was.
    std::fs::write(&settings, "{\"model\": ").expect("writes");
    let refused = install(&project, &[]);
    assert_eq!(refused.code, 2, "{refused:?}");
    assert!(refused.stderr.contains("not valid JSON"));
    assert_eq!(
        std::fs::read_to_string(&settings).expect("reads"),
        "{\"model\": "
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn the_installed_command_runs_the_hook_as_claude_code_would() {
    if !python_available() || !Path::new("/bin/sh").exists() {
        return;
    }
    let server = Server::spawn(&[]);
    let project = fresh_project("run");
    let url = server.url("");
    assert_eq!(install(&project, &["--url", &url]).code, 0);
    nudge(&server, "cc-installed", "block", "Not in this repository.");
    // Claude Code runs a command hook in a shell, with CLAUDE_PROJECT_DIR
    // set to the project root, from the session's working directory.
    let settings = read_json(&project.join(".claude/settings.json"));
    let command_line = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .expect("command")
        .to_owned();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&command_line)
        .current_dir(std::env::temp_dir());
    let project_dir = project.to_string_lossy().into_owned();
    let outcome = run_with(
        command,
        pre_tool_use("cc-installed", "rm -rf build")
            .to_string()
            .as_bytes(),
        &[("CLAUDE_PROJECT_DIR", project_dir.as_str())],
    );
    assert_eq!(outcome.code, 0, "{outcome:?}");
    assert_eq!(
        outcome.answer(),
        Some(deny(
            "Traza blocked this tool call: a person supervising run cc-installed vetoed it, \
             writing:\n  \u{201c}Not in this repository.\u{201d}"
        )),
        "the installed config.json pointed the hook at this server"
    );
    let _ = std::fs::remove_dir_all(&project);
}
