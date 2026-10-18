//! HTTP route for Agent Blame.
//!
//! - `GET /v1/blame?path=&line=&limit=&tenant=` — the earliest tool steps
//!   whose edit introduced `line` into a file matching `path`
//!   ([`traza::blame`]).
//!
//! A child module of the server binary, like `replay_routes`: it shares the
//! binary's responder, tenant binding and error mapping, and the logic lives
//! in the library.

use std::io;

use serde_json::json;
use traza::blame::{DEFAULT_BLAME_LIMIT, MAX_BLAME_LIMIT};
use traza::Store;

use super::{bind_tenant, engine_error, foreign_tenant, percent_decode, Responder, ServerMetrics};

/// The route's path.
pub(super) const BLAME: &str = "/v1/blame";

/// Serves `GET /v1/blame`.
pub(super) fn serve(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let (mut path, mut line, mut selected) = (None, None, None);
    let mut limit = DEFAULT_BLAME_LIMIT;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match percent_decode(key).as_str() {
            "path" => path = Some(value),
            "line" => line = Some(value),
            "tenant" => selected = Some(value),
            "limit" => match value.parse::<usize>() {
                Ok(parsed) if parsed > 0 => limit = parsed.min(MAX_BLAME_LIMIT),
                _ => {
                    return responder
                        .json(400, json!({"error": "limit must be a positive integer"}))
                }
            },
            other => {
                return responder.json(
                    400,
                    json!({"error": format!("unknown query parameter: {other}")}),
                )
            }
        }
    }
    let (Some(path), Some(line)) = (path, line) else {
        return responder.json(
            400,
            json!({"error": "path and line are required: /v1/blame?path=<file or suffix>&line=<exact line text>"}),
        );
    };
    let effective = match bind_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.blame(effective.as_deref(), &path, &line, limit) {
        Ok(blame) => responder.json(
            200,
            serde_json::to_value(blame).unwrap_or_else(|_| json!({})),
        ),
        Err(error) => engine_error(responder, metrics, error),
    }
}
