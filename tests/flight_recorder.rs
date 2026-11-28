//! The flight-recorder features against each other, through the real server:
//! a quota refusal is decided before the run guard sees a batch, an admitted
//! batch is both charged and judged, a credential minted at runtime drives a
//! replay through the Anthropic SDK's `x-api-key`, and every unbound-only
//! surface added this release — the ledger's operator routes, anchors, the
//! guard's tenant rule — refuses a bound credential with the `required`
//! capability named, the way the platform's own surfaces do.
//!
//! Each feature proves itself in its own suite; what is proved here is only
//! what none of them can see alone: the seams where they meet.

mod common;

use common::{header, Server};
use serde_json::{json, Value};

/// One LLM step of `run`, for `tenant`, recorded with its prompt and answer
/// so a replay can serve it.
fn llm_step(tenant: &str, run: &str, id: &str, start: u64) -> Value {
    json!({
        "trace_id": format!("t-{run}"), "span_id": id, "name": "chat",
        "service": "agent", "$tenant": tenant, "status": "ok",
        "start_time_ns": start, "end_time_ns": start + 100,
        "attributes": {
            "session.id": run,
            "gen_ai.operation.name": "chat",
            "gen_ai.system": "anthropic",
            "gen_ai.request.model": "claude-sonnet-4-5",
            "gen_ai.usage.input_tokens": 12,
            "gen_ai.usage.output_tokens": 4,
            "gen_ai.input.messages": [{"role": "user", "content": format!("step {id}")}],
            "gen_ai.output.messages": [{"role": "assistant", "content": format!("answer {id}"),
                                        "finish_reason": "stop"}],
        }
    })
}

fn steps(tenant: &str, run: &str, first: u64, count: u64) -> Value {
    Value::Array(
        (first..first + count)
            .map(|index| llm_step(tenant, run, &format!("s{index}"), 1_000 * (index + 1)))
            .collect(),
    )
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Ingests natively; returns status, the `Traza-Fuse` header, and the body.
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

#[test]
fn a_quota_refusal_is_decided_before_the_guard_sees_the_batch() {
    let server = Server::spawn(&["--fuse-max-steps", "4", "--fuse-action", "stop"]);
    let (status, body) = server.json(
        "POST",
        "/v1/admin/tenants/acme/limits",
        Some(&json!({"monthly_spans": 5, "monthly_bytes": null,
                     "ingest_paused": false, "ttl_seconds": null})),
        &[],
    );
    assert_eq!(status, 200, "{body}");

    // Admitted: charged to acme's usage AND judged by the guard, in one ack.
    let (status, fuse, body) = ingest(&server, &steps("acme", "run-1", 0, 4), &[]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(fuse.as_deref(), Some("stop; run=run-1"), "{body}");
    assert_eq!(body["fuses"][0]["verdict"], "stop", "{body}");
    assert_eq!(body["fuses"][0]["tenant"], "acme", "{body}");
    let (_, usage) = server.json("GET", "/v1/usage?tenant=acme", None, &[]);
    assert_eq!(
        usage["tenants"][0]["lifetime"]["spans"],
        json!(4),
        "{usage}"
    );
    assert_eq!(usage["tenants"][0]["month"]["spans"], json!(4), "{usage}");

    // Refused by the quota: 4 + 2 > 5. The batch is counted as rejected and
    // carries no verdict — the guard never saw it, so the run's step count
    // is still the admitted four, not six.
    let (status, fuse, body) = ingest(&server, &steps("acme", "run-1", 4, 2), &[]);
    assert_eq!(status, 429, "{body}");
    assert_eq!(fuse, None, "a refused batch raises no fuse signal");
    assert_eq!(body["quota"]["dimension"], "monthly_spans", "{body}");
    let (status, run) = server.json("GET", "/v1/fuses/runs/run-1?tenant=acme", None, &[]);
    assert_eq!(status, 200, "{run}");
    assert_eq!(run["usage"]["steps"], json!(4), "{run}");
    assert_eq!(run["usage"]["llm_calls"], json!(4), "{run}");
    let (_, usage) = server.json("GET", "/v1/usage?tenant=acme", None, &[]);
    let row = &usage["tenants"][0];
    assert_eq!(row["lifetime"]["spans"], json!(4), "{usage}");
    assert_eq!(row["lifetime"]["rejected"], json!(2), "{usage}");
    assert_eq!(row["state"], "warning", "{usage}");

    // A pause refuses before the guard too: another run never starts.
    let (status, body) = server.json(
        "POST",
        "/v1/admin/tenants/acme/limits",
        Some(&json!({"monthly_spans": null, "monthly_bytes": null,
                     "ingest_paused": true, "ttl_seconds": null})),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    let (status, _, body) = ingest(&server, &steps("acme", "run-2", 0, 4), &[]);
    assert_eq!(status, 429, "{body}");
    let (status, runs) = server.json("GET", "/v1/fuses/runs?tenant=acme", None, &[]);
    assert_eq!(status, 200, "{runs}");
    let listed: Vec<&str> = runs["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|run| run["run"].as_str().expect("run"))
        .collect();
    assert_eq!(listed, ["run-1"], "{runs}");
}

#[test]
fn a_runtime_credential_drives_a_replay_through_x_api_key() {
    let server = Server::spawn(&[]);
    // Open mode: the first credential must be an unbound admin, after which
    // auth is enforced and everything below authenticates.
    let (status, admin) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "admin", "label": "operator"})),
        &[],
    );
    assert_eq!(status, 201, "{admin}");
    let admin = bearer(admin["token"].as_str().expect("token"));
    let auth = [("Authorization", admin.as_str())];
    let (status, reader) = server.json(
        "POST",
        "/v1/admin/credentials",
        Some(&json!({"scope": "ro", "tenant": "acme", "label": "harness"})),
        &auth,
    );
    assert_eq!(status, 201, "{reader}");
    let reader_id = reader["id"].as_str().expect("id").to_owned();
    let reader = reader["token"].as_str().expect("token").to_owned();
    let (status, _, body) = ingest(&server, &steps("acme", "sess-1", 0, 2), &auth);
    assert_eq!(status, 200, "{body}");

    // A bound `ro` credential may create the replay: replays are reads.
    let reader_bearer = bearer(&reader);
    let (status, created) = server.json(
        "POST",
        "/v1/replays",
        Some(&json!({"run": "sess-1", "mode": "loose"})),
        &[("Authorization", reader_bearer.as_str())],
    );
    assert_eq!(status, 201, "{created}");
    let messages = format!(
        "{}/v1/messages",
        created["base_urls"]["anthropic"]
            .as_str()
            .expect("base url")
    );
    let request = json!({"model": "claude-sonnet-4-5", "max_tokens": 64,
                         "messages": [{"role": "user", "content": "step s0"}]});

    // The Anthropic SDK presents the minted token as x-api-key, and the
    // credential store — not TRAZA_TOKENS — recognises it.
    let (status, message) = server.json(
        "POST",
        &messages,
        Some(&request),
        &[("x-api-key", reader.as_str())],
    );
    assert_eq!(status, 200, "{message}");
    assert_eq!(message["type"], "message", "{message}");
    assert_eq!(message["content"][0]["text"], "answer s0", "{message}");

    // x-api-key is confined to the replay routes: elsewhere it is no
    // credential at all.
    let (status, _) = server.json(
        "GET",
        "/v1/spans?limit=1",
        None,
        &[("x-api-key", reader.as_str())],
    );
    assert_eq!(status, 401);

    // A revoked credential stops working through x-api-key as well.
    let (status, body) = server.json(
        "POST",
        &format!("/v1/admin/credentials/{reader_id}/revoke"),
        None,
        &auth,
    );
    assert_eq!(status, 200, "{body}");
    let (status, _) = server.json(
        "POST",
        &messages,
        Some(&request),
        &[("x-api-key", reader.as_str())],
    );
    assert_eq!(status, 401);
}

#[test]
fn bound_credentials_are_refused_every_unbound_surface_with_the_grant_named() {
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            "admin:root,rw:writer,ro@acme:acme-ro,admin@acme:acme-admin",
        )],
    );
    let as_ = |token: &str| format!("Bearer {token}");
    let forbidden = |required: &str| json!({"error": "forbidden", "required": required});

    // The ledger's operator routes are unbound-only, whatever the scope.
    for (method, path, token) in [
        ("GET", "/v1/ledger", "acme-ro"),
        ("GET", "/v1/ledger/verify", "acme-ro"),
        ("GET", "/v1/ledger/verify", "acme-admin"),
        ("POST", "/v1/ledger/anchors", "acme-admin"),
    ] {
        let auth = as_(token);
        let (status, body) = server.json(method, path, None, &[("Authorization", auth.as_str())]);
        assert_eq!(status, 403, "{method} {path} as {token}: {body}");
        assert_eq!(body, forbidden("unbound"), "{method} {path} as {token}");
    }
    // Anchoring needs admin as well: an unbound rw collector is refused for
    // the grant it lacks.
    let auth = as_("writer");
    let (status, body) = server.json(
        "POST",
        "/v1/ledger/anchors",
        Some(&json!({})),
        &[("Authorization", auth.as_str())],
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body, forbidden("admin"));

    // What a bound credential may read: the head, and its own evidence.
    let auth = as_("acme-ro");
    let (status, head) = server.json(
        "GET",
        "/v1/ledger/head",
        None,
        &[("Authorization", auth.as_str())],
    );
    assert_eq!(status, 200, "{head}");
    let (status, pack) = server.json(
        "GET",
        "/v1/evidence",
        None,
        &[("Authorization", auth.as_str())],
    );
    assert_eq!(status, 200, "{pack}");
    assert_eq!(pack["tenant"], "acme");
    assert_eq!(pack["usage"]["tenant"], "acme", "{pack}");

    // Naming another tenant answers the one cross-tenant body on every
    // surface — guard, evidence and replay alike.
    let foreign = json!({"error": "this credential is bound to a different tenant",
                         "required": "unbound"});
    for path in [
        "/v1/fuses/runs/r?tenant=bigco",
        "/v1/fuses/policies?tenant=bigco",
        "/v1/evidence?tenant=bigco",
        "/v1/tapes/r?tenant=bigco",
    ] {
        let (status, body) = server.json("GET", path, None, &[("Authorization", auth.as_str())]);
        assert_eq!(status, 403, "{path}: {body}");
        assert_eq!(body, foreign, "{path}");
    }

    // And /v1/admin/* stays default-deny for a bound admin.
    let auth = as_("acme-admin");
    let (status, body) = server.json(
        "GET",
        "/v1/admin/credentials",
        None,
        &[("Authorization", auth.as_str())],
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body, forbidden("unbound"));
}
