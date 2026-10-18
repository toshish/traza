//! The X-Ray and Cache Autopsy HTTP routes, without the transport.
//!
//! `traza-server` owns sockets, framing and authentication; it hands every
//! `/v1/xray/*` and `/v1/cache/*` request here with the caller's tenant
//! binding and writes back the [`Reply`] — the same reply type, and the same
//! binding rules, as the run guard's routes ([`crate::guard::http`]):
//!
//! - `GET /v1/xray/{trace_id}/{span_id}?tenant=&resolve=&stale_steps=&ttl_seconds=`
//! - `GET /v1/cache/{run}?tenant=&ttl_seconds=&resolve=`
//!
//! A bound credential reads only its tenant, and naming another is `403`.
//! Unknown or repeated query parameters are refused, so a typo fails loudly.

use std::collections::HashMap;

use serde_json::json;

use super::{AutopsyOptions, XrayMiss, XrayOptions};
use crate::guard::http::Reply;
use crate::{Error, Store};

const XRAY_PREFIX: &str = "/v1/xray/";
const CACHE_PREFIX: &str = "/v1/cache/";

/// Largest `stale_steps` a request may name.
const MAX_STALE_STEPS: usize = 100_000;

/// Largest `ttl_seconds` a request may name: a day.
const MAX_TTL_SECONDS: u64 = 86_400;

/// Whether `path` is one of these routes.
pub fn handles(path: &str) -> bool {
    path.starts_with(XRAY_PREFIX) || path.starts_with(CACHE_PREFIX)
}

/// Serves one request. `bound` is the credential's tenant binding, `None` for
/// an unbound credential or an open server. The default `stale_steps` is the
/// store's [`crate::Config::xray_stale_steps`] (`--xray-stale-steps`).
pub fn serve(store: &Store, method: &str, path: &str, query: &str, bound: Option<&str>) -> Reply {
    let routed = match method {
        "GET" => {
            if let Some(rest) = path.strip_prefix(XRAY_PREFIX) {
                xray(store, rest, query, bound)
            } else if let Some(run) = path.strip_prefix(CACHE_PREFIX) {
                cache(store, run, query, bound)
            } else {
                Err(error(404, "not found"))
            }
        }
        _ => Err(error(404, "not found")),
    };
    routed.unwrap_or_else(|reply| reply)
}

type Routed = Result<Reply, Reply>;

fn reply(status: u16, body: serde_json::Value) -> Reply {
    Reply {
        status,
        body,
        deadline_exceeded: false,
    }
}

fn error(status: u16, message: impl Into<String>) -> Reply {
    reply(status, json!({"error": message.into()}))
}

/// The server's uniform engine-error mapping (see `guard::http`).
fn engine(failure: Error) -> Reply {
    match failure {
        Error::Invalid(reason) => error(400, reason),
        Error::InvalidSpan(reason) => error(400, reason),
        Error::Conflict(reason) => error(409, reason),
        failure @ Error::DeadlineExceeded(_) => Reply {
            deadline_exceeded: true,
            ..error(400, failure.to_string())
        },
        failure @ Error::QueryTooBroad(_) => error(400, failure.to_string()),
        other => error(503, other.to_string()),
    }
}

/// The tenant scope: the binding (refusing another named one), else the
/// named tenant, else every tenant (`None`).
fn scope(named: Option<&String>, bound: Option<&str>) -> Result<Option<String>, Reply> {
    match (bound, named) {
        (Some(bound), Some(named)) if named != bound => Err(reply(
            403,
            json!({
                "error": "this credential is bound to a different tenant",
                "required": "unbound",
            }),
        )),
        (Some(bound), _) => Ok(Some(bound.to_owned())),
        (None, None) => Ok(None),
        (None, Some(named)) => {
            if !named.is_empty() && !crate::valid_tenant(named) {
                return Err(error(
                    400,
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
                ));
            }
            Ok(Some(named.clone()))
        }
    }
}

/// Query parameters, refusing any not in `allowed` and any repeated one.
fn params(query: &str, allowed: &[&str]) -> Result<HashMap<String, String>, Reply> {
    let mut found = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !allowed.contains(&key.as_str()) {
            return Err(error(400, format!("unknown query parameter: {key}")));
        }
        if found.insert(key.clone(), percent_decode(value)).is_some() {
            return Err(error(400, format!("{key} is given twice")));
        }
    }
    Ok(found)
}

fn boolean(params: &HashMap<String, String>, key: &str, default: bool) -> Result<bool, Reply> {
    match params.get(key).map(String::as_str) {
        None => Ok(default),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(_) => Err(error(400, format!("{key} must be true or false"))),
    }
}

fn bounded(
    params: &HashMap<String, String>,
    key: &str,
    default: u64,
    max: u64,
) -> Result<u64, Reply> {
    match params.get(key) {
        None => Ok(default),
        Some(raw) => match raw.parse::<u64>() {
            Ok(value) if (1..=max).contains(&value) => Ok(value),
            _ => Err(error(
                400,
                format!("{key} must be an integer from 1 to {max}"),
            )),
        },
    }
}

fn xray(store: &Store, rest: &str, query: &str, bound: Option<&str>) -> Routed {
    const SHAPE: &str =
        "expected /v1/xray/{trace_id}/{span_id} (percent-encode a '/' inside an id)";
    let Some((trace, span)) = rest.split_once('/') else {
        return Err(error(400, SHAPE));
    };
    if span.contains('/') {
        return Err(error(400, SHAPE));
    }
    let (trace, span) = (percent_decode(trace), percent_decode(span));
    if trace.is_empty() || span.is_empty() {
        return Err(error(400, SHAPE));
    }
    let params = params(query, &["tenant", "resolve", "stale_steps", "ttl_seconds"])?;
    let tenant = scope(params.get("tenant"), bound)?;
    let options = XrayOptions {
        resolve: boolean(&params, "resolve", true)?,
        stale_steps: bounded(
            &params,
            "stale_steps",
            store.xray_stale_steps() as u64,
            MAX_STALE_STEPS as u64,
        )? as usize,
        ttl_seconds: bounded(
            &params,
            "ttl_seconds",
            super::DEFAULT_CACHE_TTL_SECONDS,
            MAX_TTL_SECONDS,
        )?,
    };
    match store
        .xray(tenant.as_deref(), &trace, &span, options)
        .map_err(engine)?
    {
        Ok(found) => Ok(reply(
            200,
            serde_json::to_value(found).unwrap_or_else(|_| json!({})),
        )),
        Err(XrayMiss::NoSpan) => Err(error(404, "span not found")),
        Err(XrayMiss::NotAnLlmStep(kind)) => Err(error(
            400,
            format!("the span is a {kind} step; the X-Ray reads model calls (llm steps)"),
        )),
        Err(XrayMiss::AmbiguousTenant(count)) => Err(error(
            400,
            format!("that span exists in {count} tenants; name one with tenant="),
        )),
    }
}

fn cache(store: &Store, run: &str, query: &str, bound: Option<&str>) -> Routed {
    let run = percent_decode(run);
    if run.is_empty() {
        return Err(error(400, "expected /v1/cache/{run}"));
    }
    let params = params(query, &["tenant", "ttl_seconds", "resolve"])?;
    let tenant = scope(params.get("tenant"), bound)?;
    let options = AutopsyOptions {
        ttl_seconds: bounded(
            &params,
            "ttl_seconds",
            super::DEFAULT_CACHE_TTL_SECONDS,
            MAX_TTL_SECONDS,
        )?,
        resolve: boolean(&params, "resolve", true)?,
    };
    match store
        .cache_autopsy(tenant.as_deref(), &run, options)
        .map_err(engine)?
    {
        Some(autopsy) => Ok(reply(
            200,
            serde_json::to_value(autopsy).unwrap_or_else(|_| json!({})),
        )),
        None => Err(error(404, "run not found")),
    }
}

/// `%XX` decoding, with `+` left as itself; invalid escapes stay literal.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                out.push((high * 16 + low) as u8);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_and_scoping_follow_the_server_rules() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("bad%zz%"), "bad%zz%");
        assert_eq!(scope(None, None).unwrap(), None);
        assert_eq!(
            scope(Some(&"acme".to_owned()), None).unwrap(),
            Some("acme".to_owned())
        );
        assert_eq!(scope(None, Some("acme")).unwrap(), Some("acme".to_owned()));
        let refused = scope(Some(&"beta".to_owned()), Some("acme")).unwrap_err();
        assert_eq!(refused.status, 403);
        assert_eq!(refused.body["required"], json!("unbound"));
        assert_eq!(
            scope(Some(&"Bad Tenant".to_owned()), None)
                .unwrap_err()
                .status,
            400
        );
        assert!(handles("/v1/xray/t/s") && handles("/v1/cache/run") && !handles("/v1/tapes/x"));
    }

    #[test]
    fn parameters_are_checked() {
        let unknown = params("bogus=1", &["tenant"]).unwrap_err();
        assert_eq!(unknown.status, 400);
        assert!(unknown.body["error"].as_str().unwrap().contains("bogus"));
        assert_eq!(
            params("tenant=a&tenant=b", &["tenant"]).unwrap_err().status,
            400
        );
        let parsed = params("resolve=0&ttl_seconds=60", &["resolve", "ttl_seconds"]).unwrap();
        assert!(!boolean(&parsed, "resolve", true).unwrap());
        assert_eq!(bounded(&parsed, "ttl_seconds", 300, 86_400).unwrap(), 60);
        let zero = params("ttl_seconds=0", &["ttl_seconds"]).unwrap();
        assert_eq!(
            bounded(&zero, "ttl_seconds", 300, 86_400)
                .unwrap_err()
                .status,
            400
        );
    }
}
