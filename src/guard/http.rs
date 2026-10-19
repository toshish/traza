//! The run guard's HTTP routes, without the transport.
//!
//! `traza-server` owns sockets, framing and authentication; it hands every
//! `/v1/fuses/*` request here with the caller's tenant binding and writes
//! back the [`Reply`]. Keeping the routes beside the engine API they call —
//! as the MCP server is — keeps the binary's dispatcher to one arm, and lets
//! the routes' tenant rules be tested without a process.
//!
//! The binding rules are the server's everywhere else: a bound credential is
//! forced to its tenant, and naming another one is `403 {"error": "this
//! credential is bound to a different tenant", "required": "unbound"}`.
//! Unknown query parameters and unknown body fields are refused, so a typo
//! fails loudly.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{Limits, NextStep, PolicySpec, RunListFilter, GLOBAL_POLICY_NAME};
use crate::{Error, Store};

/// Default and maximum `limit` for a run listing.
const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1000;

/// What to answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Value,
    /// Whether the request exhausted the query deadline, for the server's
    /// `traza_query_deadline_exceeded_total`.
    pub deadline_exceeded: bool,
}

impl Reply {
    pub(crate) fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            deadline_exceeded: false,
        }
    }

    pub(crate) fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": message.into()}),
            deadline_exceeded: false,
        }
    }

    /// The server's one cross-tenant refusal, byte for byte: `required`
    /// says what would have worked, so a dashboard can name it.
    pub(crate) fn foreign_tenant() -> Self {
        Self {
            status: 403,
            body: json!({
                "error": "this credential is bound to a different tenant",
                "required": "unbound",
            }),
            deadline_exceeded: false,
        }
    }

    /// The server's uniform engine-error mapping: the caller's defects are
    /// 400, conflicts 409, an exhausted budget 400 (narrowing, not retrying,
    /// is the remedy), anything else the store's 503. A run too long for a
    /// rebuild to read whole is 400 too, as a too-broad query is on the
    /// search route: retrying cannot help, and 503 would say it might.
    pub(crate) fn engine(error: Error) -> Self {
        match error {
            Error::Invalid(reason) => Self::error(400, reason),
            Error::InvalidSpan(reason) => Self::error(400, reason),
            Error::Conflict(reason) => Self::error(409, reason),
            error @ Error::DeadlineExceeded(_) => Self {
                deadline_exceeded: true,
                ..Self::error(400, error.to_string())
            },
            error @ Error::QueryTooBroad(_) => Self::error(400, error.to_string()),
            other => Self::error(503, other.to_string()),
        }
    }
}

pub(crate) type Routed<T> = Result<T, Reply>;

/// Serves one `/v1/fuses/*` request. `bound` is the credential's tenant
/// binding, `None` for an unbound credential or an open server. The caller
/// has already authorized the method: `GET` and `POST /v1/fuses/check` need
/// read access, every other `POST` write access.
pub fn serve(
    store: &Store,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    bound: Option<&str>,
) -> Reply {
    // Ids are cut out with strip_prefix/strip_suffix, never by index: a path
    // like `/v1/fuses/runs/trip` both starts with the prefix and ends with
    // the suffix, and slicing between the two would run backwards.
    let between = |prefix: &str, suffix: &str| {
        path.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .filter(|id| !id.is_empty())
    };
    let routed = match (method, path) {
        ("POST", "/v1/fuses/check") => check(store, query, body, bound),
        ("GET", "/v1/fuses/policies") => list_policies(store, query, bound),
        ("POST", "/v1/fuses/policies") => put_policy(store, query, body, bound),
        ("GET", "/v1/fuses/runs") => list_runs(store, query, bound),
        ("GET", "/v1/fuses/events") => events(store, query, bound),
        ("POST", _) => {
            if let Some(id) = between("/v1/fuses/policies/", "/delete") {
                delete_policy(store, &percent_decode(id), query, bound)
            } else if let Some(run) = between("/v1/fuses/runs/", "/trip") {
                trip(store, &percent_decode(run), query, body, bound)
            } else if let Some(run) = between("/v1/fuses/runs/", "/reset") {
                reset(store, &percent_decode(run), query, body, bound)
            } else {
                Err(Reply::error(404, "not found"))
            }
        }
        ("GET", _) => match between("/v1/fuses/runs/", "") {
            Some(run) => verdict(store, &percent_decode(run), query, bound),
            None => Err(Reply::error(404, "not found")),
        },
        _ => Err(Reply::error(404, "not found")),
    };
    routed.unwrap_or_else(|reply| reply)
}

fn verdict(store: &Store, run: &str, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let doc = store.fuse_verdict(&tenant, run).map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&doc)))
}

fn check(store: &Store, query: &str, body: &[u8], bound: Option<&str>) -> Routed<Reply> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Check {
        run: String,
        #[serde(default)]
        tenant: Option<String>,
        #[serde(default)]
        next: Option<NextStep>,
    }
    params(query, &[])?;
    let request: Check = parse_body(body, "{\"run\": \"...\", \"next\"?: {...}}")?;
    let tenant = run_tenant(request.tenant.as_ref(), bound)?;
    let doc = match &request.next {
        Some(next) => store.fuse_check(&tenant, &request.run, next),
        None => store.fuse_verdict(&tenant, &request.run),
    }
    .map_err(Reply::engine)?;
    // A pre-call check — one naming its next step — is activity, and the
    // run is on that step (the live view); one without is a verdict read.
    let pre_step = request.next.is_some();
    if let Some(next) = &request.next {
        store.guard_note_check(&tenant, &request.run, next);
    }
    // Pending nudges ride a pre-call check's verdict, delivered once; a
    // `block` vetoes this step (see `crate::steer::check_reply`).
    let body = crate::steer::check_reply(store, &tenant, &request.run, &doc, pre_step)
        .map_err(Reply::engine)?;
    // Always 200 when there is a verdict to give: the verdict is the answer,
    // and a pre-call hook should not have to tell "stop" from a failure by
    // status code.
    Ok(Reply::ok(body))
}

fn list_runs(store: &Store, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["state", "limit", "tenant"])?;
    let tenant = scope_tenant(params.get("tenant"), bound)?;
    let filter = match params.get("state") {
        None => RunListFilter::Active,
        Some(state) => RunListFilter::parse(state)
            .ok_or_else(|| Reply::error(400, "state must be stop|warn|active"))?,
    };
    let limit = match params.get("limit") {
        None => DEFAULT_LIST_LIMIT,
        Some(limit) => limit
            .parse::<usize>()
            .map_err(|_| Reply::error(400, "invalid limit"))?
            .min(MAX_LIST_LIMIT),
    };
    let docs = store
        .fuse_runs(tenant.as_deref(), filter, limit)
        .map_err(Reply::engine)?;
    Ok(Reply::ok(json!({"runs": to_json(&docs)})))
}

fn trip(store: &Store, run: &str, query: &str, body: &[u8], bound: Option<&str>) -> Routed<Reply> {
    #[derive(Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Trip {
        #[serde(default)]
        reason: String,
    }
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let request: Trip = parse_optional_body(body, "{\"reason\": \"...\"}")?;
    let doc = store
        .fuse_trip(&tenant, run, &request.reason)
        .map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&doc)))
}

fn reset(store: &Store, run: &str, query: &str, body: &[u8], bound: Option<&str>) -> Routed<Reply> {
    #[derive(Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reset {
        #[serde(default)]
        grant: Option<Limits>,
    }
    let params = params(query, &["tenant"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let request: Reset = parse_optional_body(body, "{\"grant\"?: {limits}}")?;
    let doc = store
        .fuse_reset(&tenant, run, request.grant)
        .map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&doc)))
}

fn list_policies(store: &Store, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["tenant"])?;
    let tenant = scope_tenant(params.get("tenant"), bound)?;
    let policies = store
        .fuse_policies(tenant.as_deref())
        .map_err(Reply::engine)?;
    let (limits, action) = store.fuse_global_policy().map_err(Reply::engine)?;
    Ok(Reply::ok(json!({
        "policies": to_json(&policies),
        // The fallback every tenant without a matching or `default` policy
        // gets, so a reader can see the whole resolution, not two thirds.
        "global": {"name": GLOBAL_POLICY_NAME, "limits": to_json(&limits), "action": action},
    })))
}

fn put_policy(store: &Store, query: &str, body: &[u8], bound: Option<&str>) -> Routed<Reply> {
    params(query, &[])?;
    let mut spec: PolicySpec = parse_body(
        body,
        "{\"name\": \"...\", \"match\"?: {\"service\"?}, \"limits\": {...}, \"action\"?: \"stop|warn\"}",
    )?;
    let named = (!spec.tenant.is_empty()).then(|| spec.tenant.clone());
    spec.tenant = run_tenant(named.as_ref(), bound)?;
    let policy = store.put_fuse_policy(spec).map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&policy)))
}

fn delete_policy(store: &Store, id: &str, query: &str, bound: Option<&str>) -> Routed<Reply> {
    params(query, &[])?;
    match store.delete_fuse_policy(bound, id).map_err(Reply::engine)? {
        Some(policy) => Ok(Reply::ok(
            json!({"deleted": true, "policy": to_json(&policy)}),
        )),
        // Another tenant's policy is answered exactly like a missing one:
        // its existence is a fact about that tenant.
        None => Err(Reply::error(404, format!("no fuse policy {id}"))),
    }
}

fn events(store: &Store, query: &str, bound: Option<&str>) -> Routed<Reply> {
    let params = params(query, &["after", "wait_ms", "tenant"])?;
    let tenant = scope_tenant(params.get("tenant"), bound)?;
    let wait_ms = match params.get("wait_ms") {
        None => 0,
        Some(wait) => wait
            .parse::<u64>()
            .ok()
            .filter(|wait| *wait <= super::MAX_WAIT_MS)
            .ok_or_else(|| {
                Reply::error(400, format!("wait_ms must be 0..{}", super::MAX_WAIT_MS))
            })?,
    };
    let page = store
        .fuse_events(
            tenant.as_deref(),
            params.get("after").map(String::as_str),
            Duration::from_millis(wait_ms),
        )
        .map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&page)))
}

// ---------------------------------------------------------------- helpers

/// The tenant a single-run request addresses: the binding (refusing another
/// named one), else the named tenant, else the default tenant.
pub(crate) fn run_tenant(named: Option<&String>, bound: Option<&str>) -> Routed<String> {
    match (bound, named) {
        (Some(bound), Some(named)) if named != bound => Err(Reply::foreign_tenant()),
        (Some(bound), _) => Ok(bound.to_owned()),
        (None, named) => {
            let tenant = named.cloned().unwrap_or_default();
            if !tenant.is_empty() && !crate::valid_tenant(&tenant) {
                return Err(Reply::error(
                    400,
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
                ));
            }
            Ok(tenant)
        }
    }
}

/// The tenant scope a listing reads: the binding (refusing another named
/// one), else the named tenant, else every tenant (`None`).
pub(crate) fn scope_tenant(named: Option<&String>, bound: Option<&str>) -> Routed<Option<String>> {
    match (bound, named) {
        (Some(_), _) => run_tenant(named, bound).map(Some),
        (None, None) => Ok(None),
        (None, Some(_)) => run_tenant(named, None).map(Some),
    }
}

/// Query parameters, refusing any not in `allowed` and any repeated one.
pub(crate) fn params(
    query: &str,
    allowed: &[&str],
) -> Routed<std::collections::HashMap<String, String>> {
    let mut found = std::collections::HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !allowed.contains(&key.as_str()) {
            return Err(Reply::error(400, format!("unknown query parameter: {key}")));
        }
        if found.insert(key.clone(), percent_decode(value)).is_some() {
            return Err(Reply::error(400, format!("{key} is given twice")));
        }
    }
    Ok(found)
}

pub(crate) fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8], shape: &str) -> Routed<T> {
    serde_json::from_slice(body)
        .map_err(|error| Reply::error(400, format!("body must be {shape}: {error}")))
}

/// [`parse_body`], with an empty body standing for `{}`.
fn parse_optional_body<T: for<'de> Deserialize<'de> + Default>(
    body: &[u8],
    shape: &str,
) -> Routed<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    parse_body(body, shape)
}

pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| json!({}))
}

/// `%XX` and `+` decoding, as the server decodes every other path and query.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                output.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_caller_is_forced_to_its_tenant() {
        let acme = "acme".to_owned();
        let other = "other".to_owned();
        assert_eq!(run_tenant(None, Some("acme")), Ok("acme".to_owned()));
        assert_eq!(run_tenant(Some(&acme), Some("acme")), Ok("acme".to_owned()));
        assert_eq!(
            run_tenant(Some(&other), Some("acme")).map_err(|r| r.status),
            Err(403)
        );
        // The same body every other cross-tenant refusal carries, `required`
        // included, so a dashboard reads one shape from every surface.
        assert_eq!(
            run_tenant(Some(&other), Some("acme")).map_err(|r| r.body),
            Err(json!({
                "error": "this credential is bound to a different tenant",
                "required": "unbound",
            }))
        );
        assert_eq!(run_tenant(None, None), Ok(String::new()));
        assert_eq!(scope_tenant(None, None), Ok(None));
        assert_eq!(
            scope_tenant(None, Some("acme")),
            Ok(Some("acme".to_owned()))
        );
        assert_eq!(
            scope_tenant(Some(&other), None),
            Ok(Some("other".to_owned()))
        );
        let bad = "Not Valid".to_owned();
        assert_eq!(run_tenant(Some(&bad), None).map_err(|r| r.status), Err(400));
    }

    #[test]
    fn unknown_and_repeated_parameters_are_refused() {
        assert!(params("tenant=a", &["tenant"]).is_ok());
        let unknown = params("tenat=a", &["tenant"]).expect_err("typo");
        assert_eq!(
            unknown.body,
            json!({"error": "unknown query parameter: tenat"})
        );
        assert!(params("tenant=a&tenant=b", &["tenant"]).is_err());
    }

    #[test]
    fn a_verdict_no_retry_can_produce_is_a_400_not_a_503() {
        // What a hook keys on is "not 200"; what a client's retry policy
        // keys on is the class, and neither of these heals with a retry.
        for error in [
            Error::DeadlineExceeded("rebuild".into()),
            Error::QueryTooBroad("run too long to read whole".into()),
        ] {
            assert_eq!(Reply::engine(error).status, 400);
        }
        assert_eq!(Reply::engine(Error::LockPoisoned("x")).status, 503);
    }

    #[test]
    fn paths_whose_prefix_and_suffix_overlap_are_not_found_not_a_panic() {
        // Each of these starts with a route's prefix AND ends with its
        // suffix, with nothing between: slicing the id out by index panicked.
        let dir = std::env::temp_dir().join(format!(
            "traza-guard-http-{}-{}",
            std::process::id(),
            super::super::unix_now_ns()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let store = Store::open(&dir, crate::Config::default()).expect("opens");
        for path in [
            "/v1/fuses/policies/delete",
            "/v1/fuses/runs/trip",
            "/v1/fuses/runs/reset",
            "/v1/fuses/policies//delete",
            "/v1/fuses/runs//trip",
            "/v1/fuses/runs//reset",
        ] {
            let reply = serve(&store, "POST", path, "", b"", None);
            assert_eq!(reply.status, 404, "{path}: {:?}", reply.body);
        }
        let empty = serve(&store, "GET", "/v1/fuses/runs/", "", b"", None);
        assert_eq!(empty.status, 404);
        let unknown = serve(&store, "PUT", "/v1/fuses/runs/x", "", b"", None);
        assert_eq!(unknown.status, 404);
        // The real routes still resolve.
        let reply = serve(&store, "POST", "/v1/fuses/runs/x/trip", "", b"", None);
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let reply = serve(&store, "POST", "/v1/fuses/runs/x/reset", "", b"", None);
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let reply = serve(&store, "GET", "/v1/fuses/runs/x", "", b"", None);
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let path = "/v1/fuses/policies/fp_none/delete";
        let reply = serve(&store, "POST", path, "", b"", None);
        assert_eq!(reply.status, 404, "an unknown policy: {:?}", reply.body);
        assert!(reply.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("fp_none"));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn percent_decoding_matches_the_server() {
        assert_eq!(percent_decode("trace%3Aabc"), "trace:abc");
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%41"), "A");
    }
}
