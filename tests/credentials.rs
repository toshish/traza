//! Runtime credentials, process-level, plus the store-level guarantees a
//! process test cannot reach (what backups and restores carry).
//!
//! The contract: an unbound admin mints, lists and revokes credentials over
//! HTTP while the server runs; the token is shown once and stored only as a
//! digest; revocation takes effect on the next request and survives a crash;
//! environment credentials coexist; the first credential turns enforcement on
//! for good; and the credential log lives outside generations, so no backup
//! carries it and no restore rolls it back.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use common::Server;
use serde_json::{json, Value};
use traza::credentials::{CredentialStore, NewCredential};
use traza::{Config, Store};

/// `server.json` as `token`.
fn call(
    server: &Server,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let authorization = format!("Bearer {token}");
    server.json(method, path, body, &[("Authorization", &authorization)])
}

fn one_span(trace: &str) -> Value {
    json!([{"trace_id": trace, "span_id": "s1", "name": "op", "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64}])
}

fn mint(server: &Server, admin: Option<&str>, body: Value) -> Value {
    let (status, created) = match admin {
        Some(token) => call(server, token, "POST", "/v1/admin/credentials", Some(&body)),
        None => server.json("POST", "/v1/admin/credentials", Some(&body), &[]),
    };
    assert_eq!(status, 201, "{created}");
    created
}

#[test]
fn the_first_credential_bootstraps_enforcement_from_open_mode() {
    let server = Server::spawn(&[]);
    // Open mode: no credential anywhere, so the loopback server is open and
    // the caller is an unbound admin — which is what may mint the first one.
    let (status, _) = server.json("GET", "/v1/stats", None, &[]);
    assert_eq!(status, 200);

    let created = mint(
        &server,
        None,
        json!({"scope": "admin", "label": "bootstrap"}),
    );
    let token = created["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("trz_") && token.len() == 68, "{token}");
    assert!(token[4..].bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(created["prefix"], json!(&token[..12]));
    assert_eq!(created["scope"], json!("admin"));
    assert_eq!(created["tenant"], Value::Null);
    assert_eq!(created["label"], json!("bootstrap"));
    assert_eq!(created["expires_at_ns"], Value::Null);
    assert!(created["created_at_ns"].as_u64().is_some_and(|ns| ns > 0));
    let id = created["id"].as_str().expect("id");
    assert!(id.starts_with("cr_") && id.len() == 19, "{id}");

    // From the very next request, the store is closed.
    let (status, body) = server.json("GET", "/v1/stats", None, &[]);
    assert_eq!(status, 401, "{body}");
    let (status, _) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "rw"})),
        &[],
    );
    assert_eq!(status, 401, "open mode ended with the first credential");
    let (status, _) = call(&server, &token, "GET", "/v1/stats", None);
    assert_eq!(status, 200);

    // The token is on the wire exactly once: the list shows the prefix, never
    // the token or its digest, and the log on disk holds only the digest.
    let (status, listed) = call(&server, &token, "GET", "/v1/admin/credentials", None);
    assert_eq!(status, 200);
    let rows = listed["credentials"].as_array().expect("credentials");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let mut keys: Vec<&str> = row
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "created_at_ns",
            "expires_at_ns",
            "id",
            "label",
            "last_used_at_ns",
            "prefix",
            "revoked_at_ns",
            "scope",
            "source",
            "tenant"
        ]
    );
    assert_eq!(row["source"], json!("api"));
    assert_eq!(row["revoked_at_ns"], Value::Null);
    assert!(
        row["last_used_at_ns"].as_u64().is_some(),
        "it was just used"
    );
    let wire = listed.to_string();
    assert!(!wire.contains(&token[12..]), "{wire}");
    let log = std::fs::read_to_string(server.data_dir.join("credentials.jsonl")).expect("log");
    assert!(!log.contains(&token), "the token never reaches disk");
}

/// The attack this guards against: with no credential configured, a web page
/// the operator visits sends the loopback server a "simple" cross-site POST —
/// `text/plain`, no preflight — minting an admin credential with a token the
/// page chose. That would lock the operator out, hand the page a live admin
/// token, and make the credential log count as configured auth for a later
/// non-loopback start.
#[test]
fn a_cross_site_page_cannot_mint_erase_or_pin_in_open_mode() {
    let server = Server::spawn(&[]);
    let attack = br#"{"scope":"admin","token":"attacker-chosen-0123456789abcd"}"#;
    let evil = ("Origin", "https://evil.example");

    // Exactly as a browser sends it.
    let (status, _, body) = server.request(
        "POST",
        "/v1/admin/credentials",
        Some(attack),
        &[("Content-Type", "text/plain;charset=UTF-8"), evil],
    );
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("origin not allowed"), "{body}");
    // Declaring JSON does not help a foreign page (a browser would preflight,
    // and the server's own answer is the same refusal).
    let (status, _, body) = server.request(
        "POST",
        "/v1/admin/credentials",
        Some(attack),
        &[("Content-Type", "application/json"), evil],
    );
    assert_eq!(status, 403, "{body}");
    // A browser that omits Origin can still only send a form or text/plain,
    // and an admin body must be declared JSON.
    let (status, _, body) = server.request(
        "POST",
        "/v1/admin/credentials",
        Some(attack),
        &[("Content-Type", "text/plain")],
    );
    assert_eq!(status, 415, "{body}");
    assert!(body.contains("application/json"), "{body}");

    // Erasure and backups are the same request shape, refused the same way.
    let (status, _) = server.json("POST", "/v1/spans", Some(&one_span("victim")), &[]);
    assert_eq!(status, 200);
    let (status, _, body) = server.request(
        "POST",
        "/v1/erasures",
        Some(br#"{"subject":{"kind":"trace","trace_id":"victim"}}"#),
        &[("Content-Type", "text/plain"), evil],
    );
    assert_eq!(status, 403, "{body}");
    let (status, _, body) = server.request(
        "POST",
        "/v1/backups/planted",
        None,
        &[("Content-Type", "text/plain"), evil],
    );
    assert_eq!(status, 403, "{body}");

    // Nothing happened: still open, no credential log, the trace intact.
    assert!(!server.data_dir.join("credentials.jsonl").exists());
    let (_, me) = server.json("GET", "/v1/whoami", None, &[]);
    assert_eq!(me["auth"], json!("open"));
    let (status, trace) = server.json("GET", "/v1/traces/victim", None, &[]);
    assert_eq!(status, 200, "{trace}");
    // Non-admin routes are not this rule's business: a foreign Origin on
    // ingest is answered as before.
    let (status, _, body) = server.request(
        "POST",
        "/v1/spans",
        Some(one_span("other").to_string().as_bytes()),
        &[evil],
    );
    assert_eq!(status, 200, "{body}");

    // A page on this server (the dashboard) and a script with no Origin are
    // let through.
    let own_page = server.url("");
    let created = {
        let (status, created) = server.json(
            "POST",
            "/v1/admin/credentials",
            Some(&json!({"scope": "admin"})),
            &[("Origin", &own_page)],
        );
        assert_eq!(status, 201, "{created}");
        created
    };
    let token = created["token"].as_str().expect("token");
    // Once a credential exists a page cannot attach the bearer it would need,
    // so the origin no longer decides anything — the token does.
    let authorization = format!("Bearer {token}");
    let (status, _) = server.json(
        "GET",
        "/v1/admin/credentials",
        None,
        &[evil, ("Authorization", &authorization)],
    );
    assert_eq!(status, 200);
    let (status, _) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "ro"})),
        &[evil],
    );
    assert_eq!(status, 401, "and without the token, it is unauthorized");
}

#[test]
fn the_first_credential_must_be_an_unbound_admin() {
    let server = Server::spawn(&[]);
    for body in [
        json!({"scope": "ro", "tenant": "acme", "label": "acme dashboard"}),
        json!({"scope": "rw"}),
        json!({"scope": "admin", "tenant": "acme"}),
    ] {
        let (status, refused) = server.json("POST", "/v1/admin/credentials", Some(&body), &[]);
        assert_eq!(status, 409, "{body}: {refused}");
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("unbound admin")),
            "{refused}"
        );
    }
    // Refused without a trace: the store is still open and still has no log.
    let (_, me) = server.json("GET", "/v1/whoami", None, &[]);
    assert_eq!(me["auth"], json!("open"));
    assert!(!server.data_dir.join("credentials.jsonl").exists());
    let admin = mint(&server, None, json!({"scope": "admin"}));
    let admin = admin["token"].as_str().expect("token");
    mint(
        &server,
        Some(admin),
        json!({"scope": "ro", "tenant": "acme"}),
    );
}

#[test]
fn bound_and_non_admin_credentials_are_denied_every_admin_path() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:root-token,rw:writer-token,admin@acme:acme-admin-token,ro@acme:acme-reader",
        )],
    );

    // Default deny by prefix: a bound credential is refused on paths that do
    // not even exist, before any route is consulted, and the body says an
    // unbound credential is what is missing.
    for (method, path) in [
        ("GET", "/v1/admin/credentials"),
        ("POST", "/v1/admin/credentials"),
        ("POST", "/v1/admin/credentials/cr_0000000000000000/revoke"),
        ("GET", "/v1/admin/tenants/acme/limits"),
        ("POST", "/v1/admin/tenants/acme/limits"),
        ("GET", "/v1/admin/not-a-route-yet"),
        ("GET", "/v1/admin"),
    ] {
        for token in ["acme-admin-token", "acme-reader"] {
            let (status, body) = call(&server, token, method, path, Some(&json!({})));
            assert_eq!(status, 403, "{token} {method} {path}: {body}");
            assert_eq!(
                body,
                json!({"error": "forbidden", "required": "unbound"}),
                "{token} {method} {path}"
            );
        }
        // An unbound credential without admin is told it needs admin.
        let (status, body) = call(&server, "writer-token", method, path, Some(&json!({})));
        assert_eq!(status, 403, "{method} {path}");
        assert_eq!(body["required"], json!("admin"), "{method} {path}");
    }
    // The unbound admin reaches the prefix; an unknown path under it is an
    // ordinary 404.
    let (status, _) = call(
        &server,
        "root-token",
        "GET",
        "/v1/admin/not-a-route-yet",
        None,
    );
    assert_eq!(status, 404);

    // Every other refusal now names its missing capability too.
    let (status, body) = call(
        &server,
        "acme-reader",
        "POST",
        "/v1/spans",
        Some(&one_span("t")),
    );
    assert_eq!((status, &body["required"]), (403, &json!("rw")));
    let (status, body) = call(&server, "acme-reader", "GET", "/v1/stats", None);
    assert_eq!((status, &body["required"]), (403, &json!("unbound")));
    let (status, body) = call(
        &server,
        "acme-reader",
        "GET",
        "/v1/spans?tenant=bigco",
        None,
    );
    assert_eq!(status, 403);
    assert_eq!(
        body,
        json!({"error": "this credential is bound to a different tenant", "required": "unbound"})
    );
}

#[test]
fn backups_need_an_unbound_admin_now() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:root-token,rw:writer-token,admin@acme:acme-admin-token",
        )],
    );
    let (status, body) = call(&server, "writer-token", "POST", "/v1/backups/nightly", None);
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        body["required"],
        json!("admin"),
        "a pin holds disk until released: rw no longer suffices"
    );
    let (status, body) = call(
        &server,
        "acme-admin-token",
        "POST",
        "/v1/backups/nightly",
        None,
    );
    assert_eq!((status, &body["required"]), (403, &json!("unbound")));
    // Something to back up: pinning a store that has never written a file
    // fails on its own account, which is not what this test is about.
    let (status, _) = call(
        &server,
        "writer-token",
        "POST",
        "/v1/spans",
        Some(&one_span("t")),
    );
    assert_eq!(status, 200);
    let (status, body) = call(&server, "root-token", "POST", "/v1/backups/nightly", None);
    assert_eq!(status, 201, "{body}");
    let (status, _) = call(
        &server,
        "writer-token",
        "POST",
        "/v1/backups/nightly/release",
        None,
    );
    assert_eq!(status, 403);
    let (status, _) = call(
        &server,
        "root-token",
        "POST",
        "/v1/backups/nightly/release",
        None,
    );
    assert_eq!(status, 200);
}

#[test]
fn a_minted_credential_carries_its_scope_and_binding_until_revoked_even_across_a_crash() {
    let mut server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", "admin:root-token")]);
    let writer = mint(
        &server,
        Some("root-token"),
        json!({"scope": "rw", "tenant": "acme", "label": "acme collector"}),
    );
    let writer_token = writer["token"].as_str().expect("token").to_owned();
    let writer_id = writer["id"].as_str().expect("id").to_owned();
    let reader = mint(&server, Some("root-token"), json!({"scope": "ro"}));
    let reader_token = reader["token"].as_str().expect("token").to_owned();

    // The binding is real: the writer's spans are stamped acme, and it reads
    // only acme.
    let (status, body) = call(
        &server,
        &writer_token,
        "POST",
        "/v1/spans",
        Some(&one_span("t-acme")),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(&server, "root-token", "GET", "/v1/traces/t-acme", None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"][0]["$tenant"], json!("acme"));
    let (status, _) = call(
        &server,
        &reader_token,
        "POST",
        "/v1/spans",
        Some(&one_span("t-ro")),
    );
    assert_eq!(status, 403, "ro stays ro");

    // Revocation is immediate, idempotent, and named precisely.
    let path = format!("/v1/admin/credentials/{writer_id}/revoke");
    let (status, revoked) = call(&server, "root-token", "POST", &path, None);
    assert_eq!(status, 200, "{revoked}");
    assert_eq!(revoked["id"], json!(writer_id));
    let revoked_at = revoked["revoked_at_ns"].as_u64().expect("instant");
    let (status, _) = call(&server, &writer_token, "GET", "/v1/spans", None);
    assert_eq!(
        status, 401,
        "a revoked token is unknown from the next request"
    );
    let (status, again) = call(&server, "root-token", "POST", &path, None);
    assert_eq!(
        (status, again["revoked_at_ns"].as_u64()),
        (200, Some(revoked_at))
    );
    let (status, missing) = call(
        &server,
        "root-token",
        "POST",
        "/v1/admin/credentials/cr_ffffffffffffffff/revoke",
        None,
    );
    assert_eq!(status, 404, "{missing}");

    // Environment credentials are listed alongside, and are not this API's
    // to revoke.
    let (_, listed) = call(&server, "root-token", "GET", "/v1/admin/credentials", None);
    let rows = listed["credentials"].as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["source"], json!("env"));
    assert_eq!(
        rows[0]["prefix"],
        json!("ro"),
        "a quarter of a 10-char token, rounded down"
    );
    let env_id = rows[0]["id"].as_str().expect("id");
    let (status, refusal) = call(
        &server,
        "root-token",
        "POST",
        &format!("/v1/admin/credentials/{env_id}/revoke"),
        None,
    );
    assert_eq!(status, 409);
    assert_eq!(
        refusal["error"],
        json!("env credentials are revoked by restarting without them")
    );

    // kill -9: the revoke was fsynced before it was acknowledged.
    server.restart();
    let (status, _) = call(&server, &writer_token, "GET", "/v1/spans", None);
    assert_eq!(status, 401, "revocation survives a crash");
    let (status, _) = call(&server, &reader_token, "GET", "/v1/spans", None);
    assert_eq!(status, 200, "a live runtime credential survives it too");
    let (_, listed) = call(&server, "root-token", "GET", "/v1/admin/credentials", None);
    let writer_row = listed["credentials"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["id"] == json!(writer_id))
        .cloned()
        .expect("still listed");
    assert_eq!(writer_row["revoked_at_ns"], json!(revoked_at));
    assert_eq!(writer_row["tenant"], json!("acme"));
    assert_eq!(writer_row["label"], json!("acme collector"));
}

#[test]
fn supplied_tokens_and_bad_requests_are_answered_precisely() {
    let server = Server::spawn(&[]);
    let admin = mint(&server, None, json!({"scope": "admin"}));
    let admin = admin["token"].as_str().expect("token").to_owned();

    // A control plane registering a token it already issued.
    let issued = "cp_live_0123456789abcdef0123456789";
    let created = mint(
        &server,
        Some(&admin),
        json!({"scope": "rw", "token": issued}),
    );
    assert_eq!(created["token"], json!(issued));
    assert_eq!(
        created["prefix"],
        json!("cp_live_"),
        "a quarter of 34 chars"
    );
    let (status, _) = call(&server, issued, "POST", "/v1/spans", Some(&one_span("t")));
    assert_eq!(status, 200);
    let (status, body) = call(
        &server,
        &admin,
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "ro", "token": issued})),
    );
    assert_eq!(status, 409, "duplicate digest: {body}");

    for (bad, needle) in [
        (json!({"scope": "rw", "token": "too-short"}), "at least 24"),
        (
            json!({"scope": "rw", "token": "has whitespace 0123456789abcdef"}),
            "whitespace",
        ),
        (json!({"scope": "superuser"}), "scope must be"),
        (
            json!({"scope": "rw", "tenant": "Not_Valid"}),
            "tenant must be",
        ),
        (
            json!({"scope": "rw", "expires_in_seconds": 0}),
            "expires_in_seconds",
        ),
        (json!({"scope": "rw", "scopes": "typo"}), "unknown field"),
        (json!({"label": "no scope"}), "missing field"),
    ] {
        let (status, body) = call(&server, &admin, "POST", "/v1/admin/credentials", Some(&bad));
        assert_eq!(status, 400, "{bad}: {body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains(needle), "{bad}: {message}");
    }
    let (_, listed) = call(&server, &admin, "GET", "/v1/admin/credentials", None);
    assert_eq!(
        listed["credentials"].as_array().map(Vec::len),
        Some(2),
        "refusals create nothing"
    );
}

#[test]
fn an_expiring_credential_stops_authenticating_at_its_instant() {
    let server = Server::spawn(&[]);
    let admin = mint(&server, None, json!({"scope": "admin"}));
    let admin = admin["token"].as_str().expect("token").to_owned();
    let short = mint(
        &server,
        Some(&admin),
        json!({"scope": "ro", "expires_in_seconds": 1}),
    );
    let created_at = short["created_at_ns"].as_u64().expect("created");
    assert_eq!(
        short["expires_at_ns"].as_u64(),
        Some(created_at + 1_000_000_000)
    );
    let token = short["token"].as_str().expect("token").to_owned();
    // Polled rather than slept-and-asserted: the instant is what matters,
    // not how quickly this machine gets there.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (status, _) = call(&server, &token, "GET", "/v1/whoami", None);
        if status == 401 {
            break;
        }
        assert_eq!(status, 200);
        assert!(std::time::Instant::now() < deadline, "never expired");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64;
    assert!(
        now_ns >= created_at + 1_000_000_000,
        "refused only after expiry"
    );
}

#[test]
fn a_runtime_credential_counts_as_configured_auth_for_a_non_loopback_bind() {
    let dir = scratch_dir("non-loopback");
    let (outcome, refused) = start_non_loopback(&dir);
    assert!(
        matches!(outcome, Start::Exited(Some(false))),
        "{outcome:?}: {refused}"
    );
    assert!(
        refused.contains("refusing unauthenticated non-loopback bind"),
        "{refused}"
    );
    assert!(
        refused.contains("runtime credential"),
        "names the new option: {refused}"
    );

    {
        let store = Store::open(&dir, Config::default()).expect("opens");
        store
            .credentials()
            .create(
                NewCredential {
                    scope: "admin".into(),
                    ..NewCredential::default()
                },
                1,
            )
            .expect("creates");
    }
    // Past the auth check, the server either fails at bind (the usual case:
    // no host has the address) or binds and listens (a host that permits
    // non-local binds). Either way it did not refuse for want of auth.
    let (outcome, passed) = start_non_loopback(&dir);
    assert!(
        !passed.contains("refusing unauthenticated non-loopback bind"),
        "credentials.jsonl is configured auth ({outcome:?}): {passed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// How a [`start_non_loopback`] attempt ended.
#[derive(Debug)]
enum Start {
    /// The process exited; `Some(success)` when it reported a status.
    Exited(Option<bool>),
    /// It announced it was listening (and was then killed).
    Listening,
    /// It did neither within the deadline (and was then killed).
    TimedOut,
}

/// Starts the server on 192.0.2.1 — TEST-NET-1, a non-loopback address that
/// normally no host has, so a server that gets PAST the auth check usually
/// fails at bind instead, which is how this test tells the two apart without
/// opening a public socket. Returns how the attempt ended and its stderr.
///
/// Never blocks for good: on a host that allows binding a non-local address
/// (Linux with `net.ipv4.ip_nonlocal_bind=1`, as some container and CI images
/// set) the bind succeeds and the server would serve forever, so the child is
/// watched against a deadline and killed once it announces it is listening.
fn start_non_loopback(dir: &Path) -> (Start, String) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
    command
        .arg("--data-dir")
        .arg(dir)
        .arg("--host")
        .arg("192.0.2.1")
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAZA_") {
            command.env_remove(key);
        }
    }
    let mut child = command.spawn().expect("spawns traza-server");
    let stderr = child.stderr.take().expect("stderr piped");
    let (send, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    let outcome = loop {
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let listening = line.starts_with("traza-server listening on");
                seen.push(line);
                if listening {
                    break Start::Listening;
                }
                continue;
            }
            // The pipe closed: the process is exiting, or has.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Start::Exited(child.wait().ok().map(|status| status.success()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            break Start::TimedOut;
        }
    };
    if !matches!(outcome, Start::Exited(_)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = reader.join();
    seen.extend(lines.try_iter());
    (outcome, seen.join("\n"))
}

#[test]
fn backups_carry_no_credentials_and_a_restore_never_resurrects_a_revoked_token() {
    let root = scratch_dir("restore-root");
    let elsewhere = scratch_dir("restore-elsewhere");
    let (token, pin) = {
        let store = Store::open(&root, Config::default()).expect("opens");
        // The first credential of a store must be an unbound admin.
        let created = store
            .credentials()
            .create(
                NewCredential {
                    scope: "admin".into(),
                    ..NewCredential::default()
                },
                1,
            )
            .expect("creates");
        store
            .set_tenant_limits(
                "acme",
                traza::limits::TenantLimits {
                    monthly_spans: Some(10),
                    ..Default::default()
                },
            )
            .expect("limits");
        store
            .ingest(traza::Span {
                trace_id: "t".into(),
                span_id: "s".into(),
                tenant: String::new(),
                parent_span_id: None,
                name: "op".into(),
                start_time_ns: 1,
                end_time_ns: 2,
                status: String::new(),
                service: "svc".into(),
                attributes: Default::default(),
                events: Vec::new(),
                links: Vec::new(),
                extra: Default::default(),
            })
            .expect("ingests");
        store.flush().expect("flushes, writing usage.json too");
        store.pin_generation("before-revoke").expect("pins");
        let pin = store.pin_path("before-revoke");
        // The backup is telemetry only: no secrets, no terms, no counters.
        for name in ["credentials.jsonl", "limits.json", "usage.json"] {
            assert!(root.join(name).exists(), "{name} is in the data dir");
            assert!(!pin.join(name).exists(), "{name} must not be in a backup");
        }
        store
            .credentials()
            .revoke(&created.id, 2)
            .expect("revokes after the backup");
        (created.token, pin)
    };
    let header = format!("Bearer {token}");

    // Restoring IN PLACE replaces the data, not the credential log.
    let staged = elsewhere.join("staged");
    copy_dir(&pin, &staged);
    let restored = Store::restore(&root, &staged, Config::default()).expect("restores");
    assert!(
        restored
            .credentials()
            .authenticate(Some(&header), 3)
            .is_err(),
        "a restore must not roll a revocation back"
    );
    assert_eq!(
        restored
            .tenant_limits("acme")
            .expect("limits")
            .monthly_spans,
        Some(10),
        "nor the tenant's terms"
    );
    assert_eq!(restored.get_trace("t").expect("reads").len(), 1);
    drop(restored);

    // Restoring somewhere new brings no credentials at all.
    let fresh = elsewhere.join("fresh");
    let restored = Store::restore(&fresh, &staged, Config::default()).expect("restores");
    assert!(!restored.credentials().enforced());
    assert!(!CredentialStore::has_api_credentials(&fresh).expect("peeks"));
    drop(restored);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-credentials-it-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for entry in std::fs::read_dir(from).expect("reads") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copies");
        }
    }
}
