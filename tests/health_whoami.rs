//! Probes and identity, process-level.
//!
//! `/healthz` and `/readyz` answer before the auth gate, so an orchestrator
//! with no credential can probe a locked-down server; `/v1/whoami` tells any
//! authenticated caller exactly what its credential is — scope, binding,
//! whether it is an operator, and which credential it was — so a dashboard can
//! decide what to show without probing operator routes for a 403.

mod common;

use common::{header, Server};
use serde_json::{json, Value};

const TOKENS: &str = "rw@acme:acme-writer-token,ro:observer-token,admin:root-token";

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn probes_answer_without_credentials_even_when_auth_is_enforced() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);

    let (status, health) = server.json("GET", "/healthz", None, &[]);
    assert_eq!(status, 200);
    assert_eq!(
        health,
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );
    let (status, ready) = server.json("GET", "/readyz", None, &[]);
    assert_eq!((status, ready), (200, json!({"ready": true})));
    // A query string does not turn a probe into something else.
    let (status, _) = server.json("GET", "/healthz?verbose=1", None, &[]);
    assert_eq!(status, 200);

    // Only GET is a probe. Anything else goes through the gate like any
    // other request, so a probe path is not a way around authentication.
    let (status, headers, _) = server.request("POST", "/healthz", None, &[]);
    assert_eq!(status, 401);
    assert_eq!(header(&headers, "www-authenticate"), Some("Bearer"));
    // ... and the API behind the probes stays locked.
    let (status, _) = server.json("GET", "/v1/spans", None, &[]);
    assert_eq!(status, 401);
}

#[test]
fn whoami_in_open_mode_is_an_unbound_admin_with_no_credential() {
    let server = Server::spawn(&[]);
    let (status, body) = server.json("GET", "/v1/whoami", None, &[]);
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"auth": "open", "scope": "admin", "tenant": null, "operator": true,
               "credential": null})
    );
}

#[test]
fn whoami_describes_each_environment_credential() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);

    let (status, _) = server.json("GET", "/v1/whoami", None, &[]);
    assert_eq!(status, 401, "whoami needs a credential once auth is on");
    let (status, _) = server.json(
        "GET",
        "/v1/whoami",
        None,
        &[("Authorization", &bearer("not-a-token"))],
    );
    assert_eq!(status, 401);

    let (status, writer) = server.json(
        "GET",
        "/v1/whoami",
        None,
        &[("Authorization", &bearer("acme-writer-token"))],
    );
    assert_eq!(status, 200, "{writer}");
    assert_eq!(writer["auth"], json!("token"));
    assert_eq!(writer["scope"], json!("rw"));
    assert_eq!(writer["tenant"], json!("acme"));
    assert_eq!(
        writer["operator"],
        json!(false),
        "a bound credential is not an operator"
    );
    let id = writer["credential"]["id"].as_str().expect("an id");
    assert!(
        id.starts_with("cr_") && id.len() == 19 && id[3..].bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    assert_eq!(writer["credential"]["label"], json!(""));

    // A ro token reaches whoami (it is a GET), and being unbound makes it an
    // operator in the sense the dashboard needs: it may see operator screens.
    let (status, observer) = server.json(
        "GET",
        "/v1/whoami",
        None,
        &[("Authorization", &bearer("observer-token"))],
    );
    assert_eq!(status, 200);
    assert_eq!(
        (
            &observer["scope"],
            &observer["tenant"],
            &observer["operator"]
        ),
        (&json!("ro"), &Value::Null, &json!(true))
    );

    // Environment ids are derived, not random: the same token has the same
    // id after a restart, so a dashboard's "this is you" survives one.
    let mut server = server;
    server.restart();
    let (_, again) = server.json(
        "GET",
        "/v1/whoami",
        None,
        &[("Authorization", &bearer("acme-writer-token"))],
    );
    assert_eq!(again["credential"]["id"], json!(id));
}

#[test]
fn whoami_names_a_runtime_credential_and_its_label() {
    let server = Server::spawn(&[]);
    // The first credential must be an unbound admin; it mints the one under
    // test.
    let (status, admin) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "admin", "label": "bootstrap"})),
        &[],
    );
    assert_eq!(status, 201, "{admin}");
    let admin = bearer(admin["token"].as_str().expect("token"));
    let (status, created) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "ro", "tenant": "acme", "label": "acme dashboard"})),
        &[("Authorization", &admin)],
    );
    assert_eq!(status, 201, "{created}");
    let token = created["token"].as_str().expect("token");
    let (status, me) = server.json(
        "GET",
        "/v1/whoami",
        None,
        &[("Authorization", &bearer(token))],
    );
    assert_eq!(status, 200);
    assert_eq!(
        me,
        json!({"auth": "token", "scope": "ro", "tenant": "acme", "operator": false,
               "credential": {"id": created["id"], "label": "acme dashboard"}})
    );
}
