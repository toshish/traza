//! HTTP routes for trajectory queries (TQL) and the detector library.
//!
//! A child module of the server binary, like `replay_routes`: it shares the
//! binary's responder, tenant binding and error mapping, and the logic lives
//! in [`traza::tql`].
//!
//! - `GET /v1/trajectories/search?q=&since=&until=&agent=&outcome=&limit=&run=&cursor=&tenant=`
//!   — runs whose steps match a TQL pattern, most recent first; `cursor`
//!   continues an earlier answer from its `next_cursor`.
//! - `GET /v1/trajectories/explain?q=` — how a pattern was read (no store
//!   access).
//! - `GET /v1/detectors` — the detector library.
//! - `GET /v1/detectors/findings?since=&until=&agent=&detector=&cursor=&tenant=`
//!   — the library run over a window.
//!
//! A query the parser refuses is `400 {"error": "column N: …", "column": N}`.

use std::io;

use serde_json::{json, Value};
use traza::tql::{FindingsOptions, OutcomeFilter, Query, SearchOptions, TqlError};
use traza::Store;

use super::{bind_tenant, engine_error, foreign_tenant, percent_decode, Responder, ServerMetrics};

const SEARCH: &str = "/v1/trajectories/search";
const EXPLAIN: &str = "/v1/trajectories/explain";
const DETECTORS: &str = "/v1/detectors";
const FINDINGS: &str = "/v1/detectors/findings";

/// Whether `path` is one of this module's routes.
pub(super) fn handles(path: &str) -> bool {
    matches!(path, SEARCH | EXPLAIN | DETECTORS | FINDINGS)
}

/// Serves one of this module's routes.
pub(super) fn serve(
    responder: &mut Responder<'_>,
    method: &str,
    path: &str,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    if method != "GET" {
        return responder.json(405, json!({"error": format!("{path} accepts GET only")}));
    }
    match path {
        SEARCH => search(responder, query, engine, metrics, principal),
        EXPLAIN => explain(responder, query),
        DETECTORS => match pairs(query, &[]) {
            Ok(_) => responder.json(200, traza::tql::library()),
            Err(error) => responder.json(400, json!({"error": error})),
        },
        _ => findings(responder, query, engine, metrics, principal),
    }
}

/// Decoded query pairs, refusing a parameter not in `allowed`.
fn pairs(raw: &str, allowed: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown query parameter: {key}"));
        }
        out.push((key, percent_decode(value)));
    }
    Ok(out)
}

fn nanos(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{name} must be Unix nanoseconds"))
}

fn tql_refusal(responder: &mut Responder<'_>, error: &TqlError) -> io::Result<()> {
    responder.json(
        400,
        json!({"error": error.to_string(), "column": error.column}),
    )
}

fn compile(text: Option<String>) -> Result<Query, Result<TqlError, String>> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return Err(Err(
            "q is required: a TQL pattern, like q=tool(bash)[status=error]".to_owned(),
        ));
    };
    Query::compile(&text).map_err(Ok)
}

fn explain(responder: &mut Responder<'_>, query: &str) -> io::Result<()> {
    let pairs = match pairs(query, &["q"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let text = pairs
        .into_iter()
        .find(|(key, _)| key == "q")
        .map(|(_, v)| v);
    match compile(text) {
        Ok(compiled) => responder.json(
            200,
            serde_json::to_value(compiled.explain()).unwrap_or_else(|_| json!({})),
        ),
        Err(Ok(error)) => tql_refusal(responder, &error),
        Err(Err(message)) => responder.json(400, json!({"error": message})),
    }
}

fn search(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match pairs(
        query,
        &[
            "q", "since", "since_ns", "until", "until_ns", "agent", "outcome", "limit", "run",
            "cursor", "tenant",
        ],
    ) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let mut text = None;
    let mut selected = None;
    let mut options = SearchOptions::default();
    for (key, value) in pairs {
        let parsed: Result<(), String> = match key.as_str() {
            "q" => {
                text = Some(value);
                Ok(())
            }
            "since" | "since_ns" => nanos(&value, "since").map(|n| options.since_ns = Some(n)),
            "until" | "until_ns" => nanos(&value, "until").map(|n| options.until_ns = Some(n)),
            "agent" => {
                options.agent = Some(value);
                Ok(())
            }
            "outcome" => OutcomeFilter::parse(&value)
                .map(|outcome| options.outcome = outcome)
                .ok_or_else(|| {
                    "outcome must be any, success, failure, abandoned or unknown".to_owned()
                }),
            "limit" => match value.parse::<usize>() {
                Ok(limit) if limit > 0 => {
                    options.limit = limit.min(traza::tql::MAX_SEARCH_LIMIT);
                    Ok(())
                }
                _ => Err("limit must be a positive integer".to_owned()),
            },
            "run" => {
                options.run = Some(value);
                Ok(())
            }
            "cursor" => {
                options.cursor = Some(value);
                Ok(())
            }
            _ => {
                selected = Some(value);
                Ok(())
            }
        };
        if let Err(error) = parsed {
            return responder.json(400, json!({"error": error}));
        }
    }
    let compiled = match compile(text) {
        Ok(compiled) => compiled,
        Err(Ok(error)) => return tql_refusal(responder, &error),
        Err(Err(message)) => return responder.json(400, json!({"error": message})),
    };
    let effective = match bind_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.trajectory_search(
        effective.as_deref(),
        &compiled,
        &options,
        traza::mcp::unix_nanos_now(),
    ) {
        Ok(result) => responder.json(
            200,
            serde_json::to_value(result).unwrap_or_else(|_| json!({})),
        ),
        Err(error) => engine_error(responder, metrics, error),
    }
}

fn findings(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match pairs(
        query,
        &[
            "since", "since_ns", "until", "until_ns", "agent", "detector", "cursor", "tenant",
        ],
    ) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let mut selected = None;
    let mut options = FindingsOptions::default();
    for (key, value) in pairs {
        let parsed: Result<(), String> = match key.as_str() {
            "since" | "since_ns" => nanos(&value, "since").map(|n| options.since_ns = Some(n)),
            "until" | "until_ns" => nanos(&value, "until").map(|n| options.until_ns = Some(n)),
            "agent" => {
                options.agent = Some(value);
                Ok(())
            }
            "cursor" => {
                options.cursor = Some(value);
                Ok(())
            }
            "detector" => {
                options.detectors.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
                Ok(())
            }
            _ => {
                selected = Some(value);
                Ok(())
            }
        };
        if let Err(error) = parsed {
            return responder.json(400, json!({"error": error}));
        }
    }
    let effective = match bind_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.detector_findings(effective.as_deref(), &options, traza::mcp::unix_nanos_now()) {
        Ok(findings) => responder.json(200, serde_json::to_value(findings).unwrap_or(Value::Null)),
        Err(error) => engine_error(responder, metrics, error),
    }
}
