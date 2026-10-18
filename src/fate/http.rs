//! The Fate Map's HTTP routes, without the transport.
//!
//! `traza-server` owns sockets, framing and authentication; it hands every
//! `/v1/fate/*` request here with the caller's tenant binding and writes
//! back the [`Reply`]. Every route is a `GET`, so the server's method rule
//! already admits any valid credential.
//!
//! - `GET /v1/fate/cohorts?since=&until=&tenant=` — the cohorts.
//! - `GET /v1/fate/map?agent=&task=&since=&until=&depth=&min_support=&max_nodes=&tenant=`
//!   — one agent's Fate Map.
//! - `GET /v1/fate/runs/{run}?tenant=&min_support=` — foresight for one run.
//!
//! The binding rules are the server's everywhere else: a bound credential is
//! forced to its tenant, and naming another is `403 {"error": "this
//! credential is bound to a different tenant", "required": "unbound"}`. An
//! unbound caller naming no tenant addresses the default tenant on the
//! single-cohort and single-run routes, and every tenant on the cohort
//! listing. Unknown and repeated query parameters are `400`.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{MapRequest, DEFAULT_VERDICT_SUPPORT, MAX_MAP_NODES};
use crate::{Error, Store};

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
    fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            deadline_exceeded: false,
        }
    }

    fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": message.into()}),
            deadline_exceeded: false,
        }
    }

    fn foreign_tenant() -> Self {
        Self {
            status: 403,
            body: json!({
                "error": "this credential is bound to a different tenant",
                "required": "unbound",
            }),
            deadline_exceeded: false,
        }
    }

    /// The server's uniform engine-error mapping: the caller's defects and
    /// an exhausted budget are 400 (narrowing, not retrying, is the remedy),
    /// conflicts 409, anything else the store's 503.
    fn engine(error: Error) -> Self {
        match error {
            Error::Invalid(reason) => Self::error(400, reason),
            Error::InvalidSpan(reason) => Self::error(400, reason),
            Error::Conflict(reason) => Self::error(409, reason),
            error @ Error::DeadlineExceeded(_) => Self {
                deadline_exceeded: true,
                ..Self::error(
                    400,
                    format!(
                        "{error}; a Fate Map reads every span of its window, so narrow it \
                         with since= and until="
                    ),
                )
            },
            error @ Error::QueryTooBroad(_) => Self::error(400, error.to_string()),
            other => Self::error(503, other.to_string()),
        }
    }
}

type Routed<T> = Result<T, Reply>;

/// Whether `path` is one of these routes.
pub fn handles(path: &str) -> bool {
    path.starts_with("/v1/fate/")
}

/// Serves one `/v1/fate/*` request. `bound` is the credential's tenant
/// binding, `None` for an unbound credential or an open server; `now_ns` is
/// the request's clock.
pub fn serve(
    store: &Store,
    method: &str,
    path: &str,
    query: &str,
    bound: Option<&str>,
    now_ns: u64,
) -> Reply {
    let routed = match (method, path) {
        ("GET", "/v1/fate/cohorts") => cohorts(store, query, bound, now_ns),
        ("GET", "/v1/fate/map") => map(store, query, bound, now_ns),
        ("GET", _) => match path
            .strip_prefix("/v1/fate/runs/")
            .filter(|run| !run.is_empty())
        {
            Some(run) => foresee(store, &percent_decode(run), query, bound, now_ns),
            None => Err(Reply::error(404, "not found")),
        },
        _ => Err(Reply::error(404, "not found")),
    };
    routed.unwrap_or_else(|reply| reply)
}

fn cohorts(store: &Store, query: &str, bound: Option<&str>, now_ns: u64) -> Routed<Reply> {
    let params = params(query, &["since", "until", "tenant"])?;
    let scope = scope_tenant(params.get("tenant"), bound)?;
    let window = window(&params)?;
    let cohorts = store
        .fate_cohorts(scope.as_deref(), window, now_ns)
        .map_err(Reply::engine)?;
    Ok(Reply::ok(to_json(&cohorts)))
}

fn map(store: &Store, query: &str, bound: Option<&str>, now_ns: u64) -> Routed<Reply> {
    let params = params(
        query,
        &[
            "agent",
            "task",
            "since",
            "until",
            "depth",
            "min_support",
            "max_nodes",
            "tenant",
        ],
    )?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let Some(agent) = params.get("agent").filter(|agent| !agent.is_empty()) else {
        return Err(Reply::error(
            400,
            "agent is required: /v1/fate/map?agent=<agent> (list them with /v1/fate/cohorts)",
        ));
    };
    let mut request = MapRequest::new(agent);
    request.task = params.get("task").filter(|task| !task.is_empty()).cloned();
    request.window = window(&params)?;
    if let Some(depth) = params.get("depth") {
        request.depth = number(depth, "depth")? as usize;
    }
    if let Some(support) = params.get("min_support") {
        request.min_support = number(support, "min_support")?;
    }
    if let Some(nodes) = params.get("max_nodes") {
        let nodes = number(nodes, "max_nodes")? as usize;
        if nodes == 0 || nodes > MAX_MAP_NODES {
            return Err(Reply::error(
                400,
                format!("max_nodes must be 1 to {MAX_MAP_NODES}"),
            ));
        }
        request.max_nodes = nodes;
    }
    match store
        .fate_map(&tenant, &request, now_ns)
        .map_err(Reply::engine)?
    {
        Some(map) => Ok(Reply::ok(to_json(&map))),
        None => Err(Reply::error(
            404,
            format!(
                "no runs of agent `{}` in the window; list the cohorts with /v1/fate/cohorts",
                request.agent
            ),
        )),
    }
}

fn foresee(
    store: &Store,
    run: &str,
    query: &str,
    bound: Option<&str>,
    now_ns: u64,
) -> Routed<Reply> {
    let params = params(query, &["tenant", "min_support"])?;
    let tenant = run_tenant(params.get("tenant"), bound)?;
    let min_support = match params.get("min_support") {
        Some(support) => {
            let support = number(support, "min_support")?;
            if support == 0 {
                return Err(Reply::error(400, "min_support must be at least 1"));
            }
            support
        }
        None => DEFAULT_VERDICT_SUPPORT,
    };
    match store
        .foresee(&tenant, run, min_support, now_ns)
        .map_err(Reply::engine)?
    {
        Some(foresight) => Ok(Reply::ok(to_json(&foresight))),
        None => Err(Reply::error(404, "run not found")),
    }
}

// ---------------------------------------------------------------- helpers

fn window(params: &HashMap<String, String>) -> Routed<Option<(u64, u64)>> {
    let since = params
        .get("since")
        .map(|value| number(value, "since"))
        .transpose()?;
    let until = params
        .get("until")
        .map(|value| number(value, "until"))
        .transpose()?;
    match (since, until) {
        (None, None) => Ok(None),
        (since, until) => {
            let until = until.unwrap_or_else(crate::mcp::unix_nanos_now);
            let since = since.unwrap_or_else(|| until.saturating_sub(super::DEFAULT_WINDOW_NS));
            if since > until {
                return Err(Reply::error(400, "since must not be after until"));
            }
            Ok(Some((since, until)))
        }
    }
}

fn number(value: &str, name: &str) -> Routed<u64> {
    value
        .parse::<u64>()
        .map_err(|_| Reply::error(400, format!("{name} must be a non-negative integer")))
}

/// The tenant a single-cohort or single-run request addresses: the binding
/// (refusing another named one), else the named tenant, else the default
/// tenant.
fn run_tenant(named: Option<&String>, bound: Option<&str>) -> Routed<String> {
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
fn scope_tenant(named: Option<&String>, bound: Option<&str>) -> Routed<Option<String>> {
    match (bound, named) {
        (Some(_), _) => run_tenant(named, bound).map(Some),
        (None, None) => Ok(None),
        (None, Some(_)) => run_tenant(named, None).map(Some),
    }
}

/// Query parameters, refusing any not in `allowed` and any repeated one.
fn params(query: &str, allowed: &[&str]) -> Routed<HashMap<String, String>> {
    let mut found = HashMap::new();
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

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| json!({}))
}

/// `%XX` and `+` decoding, as the server decodes every other path and query.
fn percent_decode(input: &str) -> String {
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
    fn a_bound_caller_is_forced_to_its_tenant_and_refused_another() {
        let acme = "acme".to_owned();
        let other = "other".to_owned();
        assert_eq!(run_tenant(None, Some("acme")), Ok("acme".to_owned()));
        assert_eq!(run_tenant(Some(&acme), Some("acme")), Ok("acme".to_owned()));
        let refused = run_tenant(Some(&other), Some("acme")).expect_err("foreign");
        assert_eq!(refused.status, 403);
        assert_eq!(refused.body["required"], "unbound");
        assert_eq!(scope_tenant(None, None), Ok(None));
        assert_eq!(
            scope_tenant(None, Some("acme")),
            Ok(Some("acme".to_owned()))
        );
        let bad = "Not Valid".to_owned();
        assert_eq!(run_tenant(Some(&bad), None).map_err(|r| r.status), Err(400));
    }

    #[test]
    fn parameters_are_strict() {
        assert!(params("agent=a", &["agent"]).is_ok());
        assert_eq!(
            params("agnet=a", &["agent"]).expect_err("typo").body,
            json!({"error": "unknown query parameter: agnet"})
        );
        assert!(params("agent=a&agent=b", &["agent"]).is_err());
        assert!(number("-1", "depth").is_err());
        assert!(number("x", "depth").is_err());
        let mut both = HashMap::new();
        both.insert("since".to_owned(), "10".to_owned());
        both.insert("until".to_owned(), "5".to_owned());
        assert_eq!(window(&both).map_err(|r| r.status), Err(400));
        both.insert("until".to_owned(), "20".to_owned());
        assert_eq!(window(&both), Ok(Some((10, 20))));
    }

    #[test]
    fn a_deadline_is_a_400_that_says_how_to_narrow() {
        let reply = Reply::engine(Error::DeadlineExceeded("fold".into()));
        assert_eq!(reply.status, 400);
        assert!(reply.deadline_exceeded);
        assert!(reply.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("since="));
        assert_eq!(Reply::engine(Error::LockPoisoned("x")).status, 503);
    }
}
