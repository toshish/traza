//! Steering's HTTP routes, without the transport.
//!
//! `traza-server` hands every `/v1/runs/*` request here with the caller's
//! tenant binding — except the live stream, whose socket it owns, and which
//! asks [`stream_scope`] for its parameters. The rules are the run guard's
//! routes' ([`crate::guard::http`]), and so are the helpers: a bound
//! credential is forced to its tenant and naming another is `403`, unknown
//! parameters and body fields are refused, engine errors map the same way.
//!
//! - `POST /v1/runs/{run}/nudges` — queue a nudge (`rw`); `201`.
//! - `GET /v1/runs/{run}/nudges` — the run's nudges.
//! - `GET /v1/runs/live?tenant=&limit=` — live runs.
//! - `GET /v1/runs/{run}/topology?tenant=` — the run's agent graph.

use serde::Serialize;
use serde_json::json;

use super::{Nudge, NudgeSpec, NudgeStatus};
use crate::guard::http::{
    params, parse_body, percent_decode, run_tenant, scope_tenant, to_json, Reply, Routed,
};
use crate::Store;

/// The path every route here sits under.
pub const PREFIX: &str = "/v1/runs/";

/// The live stream's path; the server serves it, with [`stream_scope`].
pub const LIVE_STREAM: &str = "/v1/runs/live/stream";

/// Default and maximum `limit` for the live listing.
const DEFAULT_LIVE_LIMIT: usize = 100;
const MAX_LIVE_LIMIT: usize = 1000;

/// Whether `path` is one of these routes.
pub fn handles(path: &str) -> bool {
    path.starts_with(PREFIX)
}

/// A nudge as the API shows it: the nudge, and where it stands now.
#[derive(Serialize)]
struct NudgeView<'a> {
    #[serde(flatten)]
    nudge: &'a Nudge,
    status: NudgeStatus,
}

fn view(nudge: &Nudge, now: u64) -> serde_json::Value {
    to_json(&NudgeView {
        nudge,
        status: nudge.status(now),
    })
}

/// Serves one `/v1/runs/*` request. `bound` is the credential's tenant
/// binding; the caller has authorized the method (`POST` needs `rw`).
pub fn serve(
    store: &Store,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    bound: Option<&str>,
) -> Reply {
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return Reply::error(404, "not found");
    };
    let routed = match (method, rest) {
        ("GET", "live") => live(store, query, bound),
        _ => match rest.rsplit_once('/') {
            Some((run, action)) if !run.is_empty() => {
                let run = percent_decode(run);
                match (method, action) {
                    ("POST", "nudges") => create(store, &run, query, body, bound),
                    ("GET", "nudges") => list(store, &run, query, bound),
                    ("GET", "topology") => topology(store, &run, query, bound),
                    _ => Err(Reply::error(404, "not found")),
                }
            }
            _ => Err(Reply::error(404, "not found")),
        },
    };
    routed.unwrap_or_else(|reply| reply)
}

/// The tenant scope of `GET /v1/runs/live/stream`: the binding, else the
/// named tenant, else every tenant (`None`).
pub fn stream_scope(query: &str, bound: Option<&str>) -> Result<Option<String>, Reply> {
    let params = params(query, &["tenant"])?;
    scope_tenant(params.get("tenant"), bound)
}

fn create(
    store: &Store,
    run: &str,
    query: &str,
    body: &[u8],
    bound: Option<&str>,
) -> Routed<Reply> {
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let spec: NudgeSpec = parse_body(
        body,
        "{\"message\": \"...\", \"kind\": \"advice|block\", \"ttl_seconds\"?: integer}",
    )?;
    let nudge = store
        .create_nudge(&tenant, run, &spec)
        .map_err(Reply::engine)?;
    Ok(Reply {
        status: 201,
        body: view(&nudge, nudge.created_at_ns),
        deadline_exceeded: false,
    })
}

fn list(store: &Store, run: &str, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let nudges = store.nudges(&tenant, run).map_err(Reply::engine)?;
    let now = super::unix_now_ns();
    let views: Vec<serde_json::Value> = nudges.iter().map(|nudge| view(nudge, now)).collect();
    Ok(Reply::ok(
        json!({"run": run, "tenant": tenant, "nudges": views}),
    ))
}

fn live(store: &Store, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["tenant", "limit"])?;
    let tenant = scope_tenant(params.get("tenant"), bound)?;
    let limit = match params.get("limit") {
        None => DEFAULT_LIVE_LIMIT,
        Some(limit) => limit
            .parse::<usize>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| Reply::error(400, "limit must be a positive integer"))?
            .min(MAX_LIVE_LIMIT),
    };
    let page = store
        .live_runs(tenant.as_deref(), limit)
        .map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&page)))
}

fn topology(store: &Store, run: &str, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let topology = store.run_topology(&tenant, run).map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&topology)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(label: &str, fuses: bool) -> (std::path::PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!(
            "traza-steer-http-{label}-{}-{}",
            std::process::id(),
            super::super::unix_now_ns()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let mut config = crate::Config::default();
        config.fuses.enabled = fuses;
        let store = Store::open(&dir, config).expect("opens");
        (dir, store)
    }

    #[test]
    fn routes_resolve_and_refuse_what_they_should() {
        let (dir, store) = store("routes", true);
        let created = serve(
            &store,
            "POST",
            "/v1/runs/sess%2F1/nudges",
            "",
            br#"{"message": "check the cache first", "kind": "advice"}"#,
            None,
        );
        assert_eq!(created.status, 201, "{:?}", created.body);
        assert_eq!(created.body["run"], "sess/1", "the run id is decoded");
        assert_eq!(created.body["status"], "pending");
        assert_eq!(created.body["delivered_at_ns"], serde_json::Value::Null);
        let listed = serve(&store, "GET", "/v1/runs/sess%2F1/nudges", "", b"", None);
        assert_eq!(listed.status, 200);
        assert_eq!(listed.body["nudges"].as_array().map(Vec::len), Some(1));
        // A typo'd body field, kind, or parameter is refused, not ignored.
        for (body, query) in [
            (&br#"{"message": "x", "kind": "advice", "ttl": 5}"#[..], ""),
            (&br#"{"message": "x", "kind": "maybe"}"#[..], ""),
            (&br#"{"message": "x"}"#[..], ""),
            (&br#"{"message": "x", "kind": "advice"}"#[..], "tenat=a"),
        ] {
            let reply = serve(&store, "POST", "/v1/runs/r/nudges", query, body, None);
            assert_eq!(reply.status, 400, "{:?}", reply.body);
        }
        // A bound caller naming another tenant is refused.
        let foreign = serve(
            &store,
            "GET",
            "/v1/runs/r/nudges",
            "tenant=other",
            b"",
            Some("acme"),
        );
        assert_eq!(foreign.status, 403);
        for (method, path) in [
            ("GET", "/v1/runs/"),
            ("GET", "/v1/runs/r"),
            ("GET", "/v1/runs//nudges"),
            ("PUT", "/v1/runs/r/nudges"),
            ("POST", "/v1/runs/r/topology"),
            ("GET", "/v1/runs/r/unknown"),
        ] {
            let reply = serve(&store, method, path, "", b"", None);
            assert_eq!(reply.status, 404, "{method} {path}: {:?}", reply.body);
        }
        let live = serve(&store, "GET", "/v1/runs/live", "limit=0", b"", None);
        assert_eq!(live.status, 400);
        let live = serve(&store, "GET", "/v1/runs/live", "", b"", None);
        assert_eq!(live.status, 200, "{:?}", live.body);
        assert_eq!(live.body["window_ns"], 900_000_000_000_u64);
        let topology = serve(&store, "GET", "/v1/runs/nobody/topology", "", b"", None);
        assert_eq!(topology.status, 200);
        assert_eq!(topology.body["spans_read"], 0);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_fuses_live_runs_and_nudges_refuse_and_topology_works() {
        let (dir, store) = store("nofuses", false);
        for (method, path) in [("GET", "/v1/runs/live"), ("GET", "/v1/runs/r/nudges")] {
            let reply = serve(&store, method, path, "", b"", None);
            assert_eq!(reply.status, 409, "{path}: {:?}", reply.body);
        }
        let reply = serve(
            &store,
            "POST",
            "/v1/runs/r/nudges",
            "",
            br#"{"message": "x", "kind": "block"}"#,
            None,
        );
        assert_eq!(reply.status, 409);
        let topology = serve(&store, "GET", "/v1/runs/r/topology", "", b"", None);
        assert_eq!(topology.status, 200, "{:?}", topology.body);
        assert!(
            !dir.join(super::super::nudges::LOG_NAME).exists(),
            "steer.jsonl is not even created under --no-fuses"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
