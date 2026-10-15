//! HTTP routes for tapes, the replay proxy, run diffs and recall.
//!
//! A child module of the server binary, so it shares the binary's responder,
//! tenant binding and error mapping rather than copying them, while keeping
//! these routes out of the main dispatch — `serve_request` hands every path
//! [`handles`] claims to [`serve`] in one line.
//!
//! - `GET /v1/tapes/{run}?tenant=&resolve=&max_steps=` — the run's tape.
//! - `POST /v1/replays` — snapshot a run into a replay.
//! - `GET /v1/replays/{id}` — what a replay has served.
//! - `POST /v1/replays/{id}/openai/v1/chat/completions` — the next recorded
//!   model answer as an OpenAI chat completion (SSE when `stream: true`).
//! - `POST /v1/replays/{id}/anthropic/v1/messages` — the same as an Anthropic
//!   message.
//! - `POST /v1/replays/{id}/tools/next` — the next recorded tool result.
//! - `GET /v1/diff?a=&b=&tenant=` — two runs aligned.
//! - `GET /v1/recall?q=&outcome=&limit=&tenant=` — earlier runs on the same
//!   words.
//!
//! The provider-shaped routes answer errors in the provider's shape —
//! `{"error": {"type", "message"}}`, plus `"type": "error"` at the top for
//! Anthropic — because the client reading them is that provider's SDK, and
//! they mark divergence and exhaustion `x-should-retry: false`: both SDKs
//! retry a 409 by default, and retrying a request that does not match the
//! recording cannot make it match.

use std::io;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};
use traza::recall::OutcomeFilter;
use traza::replay::{Mode, Registry, ReplayError};
use traza::tape::{TapeOptions, DEFAULT_TAPE_MAX_BYTES, MAX_TAPE_SPANS};
use traza::Store;

use super::{bind_tenant, engine_error, foreign_tenant, percent_decode, Responder, ServerMetrics};

const REPLAYS: &str = "/v1/replays";
const REPLAY_PREFIX: &str = "/v1/replays/";
const TAPES_PREFIX: &str = "/v1/tapes/";
const OPENAI_CHAT: &str = "/openai/v1/chat/completions";
const ANTHROPIC_MESSAGES: &str = "/anthropic/v1/messages";
const TOOLS_NEXT: &str = "/tools/next";

/// The process's replay registry. One server is one store, so one registry;
/// it is created on first use and lives as long as the process.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// `--tape-max-bytes`, set once at startup by [`configure`].
static TAPE_MAX_BYTES: OnceLock<usize> = OnceLock::new();

/// Records the command line's `--tape-max-bytes`. Called once, before the
/// first connection is accepted.
pub(super) fn configure(tape_max_bytes: usize) {
    let _ = TAPE_MAX_BYTES.set(tape_max_bytes);
}

fn tape_max_bytes() -> usize {
    TAPE_MAX_BYTES
        .get()
        .copied()
        .unwrap_or(DEFAULT_TAPE_MAX_BYTES)
}

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

/// Whether `path` is one of this module's routes.
pub(super) fn handles(path: &str) -> bool {
    is_replay_path(path)
        || path.starts_with(TAPES_PREFIX)
        || path == "/v1/diff"
        || path == "/v1/recall"
}

/// Whether `path` is a replay route. Every one of them is a read, however it
/// is spelled — creating a replay snapshots the store and changes nothing in
/// it, and the provider-shaped call routes must be POSTs because that is what
/// an SDK sends — so the auth gate admits any valid credential, `ro`
/// included, rather than applying the method rule.
pub(super) fn is_replay_path(path: &str) -> bool {
    path == REPLAYS || path.starts_with(REPLAY_PREFIX)
}

/// The credential a request presents: its `Authorization` header, or — on
/// the routes under `/v1/replays/` only, and only when `Authorization` is
/// absent — its `x-api-key`, read as a bearer token.
///
/// `x-api-key` is how the Anthropic SDK authenticates, so this is what lets
/// `ANTHROPIC_API_KEY` hold a Traza token while the SDK's base URL points at
/// a replay. It is confined to those routes so no other surface gains a
/// second way to authenticate.
pub(super) fn presented_credential(
    authorization: Option<&str>,
    x_api_key: Option<&str>,
    path: &str,
) -> Option<String> {
    match (authorization, x_api_key) {
        (Some(header), _) => Some(header.to_owned()),
        (None, Some(key)) if path.starts_with(REPLAY_PREFIX) => Some(format!("Bearer {key}")),
        _ => None,
    }
}

/// Serves one of this module's routes.
#[allow(clippy::too_many_arguments)]
pub(super) fn serve(
    responder: &mut Responder<'_>,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let binding = principal.and_then(|principal| principal.tenant.as_deref());
    match (method, path) {
        ("GET", _) if path.starts_with(TAPES_PREFIX) => {
            let run = percent_decode(&path[TAPES_PREFIX.len()..]);
            tape(responder, &run, query, engine, metrics, principal)
        }
        ("GET", "/v1/diff") => diff(responder, query, engine, metrics, principal),
        ("GET", "/v1/recall") => recall(responder, query, engine, metrics, principal),
        ("POST", REPLAYS) => create(responder, body, engine, metrics, principal),
        (_, _) if path.starts_with(REPLAY_PREFIX) => {
            let rest = &path[REPLAY_PREFIX.len()..];
            let (id, route) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
            call(responder, method, id, route, body, binding)
        }
        _ => responder.json(404, json!({"error": "not found"})),
    }
}

// ------------------------------------------------------------------ helpers

/// Sends a response with the status's proper reason phrase and any extra
/// headers. Server-sent events go out with a declared length: the whole
/// stream is known before the first byte, so the connection can persist.
fn send(
    responder: &mut Responder<'_>,
    status: u16,
    content_type: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    responder.status = Some(status);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        body.len(),
        responder.connection_header(),
    );
    if content_type == "text/event-stream" {
        head.push_str("Cache-Control: no-cache\r\n");
    }
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    responder.raw(&head, body)
}

fn send_json(
    responder: &mut Responder<'_>,
    status: u16,
    extra: &[(&str, String)],
    body: &Value,
) -> io::Result<()> {
    let encoded = serde_json::to_vec(body)?;
    send(responder, status, "application/json", extra, &encoded)
}

/// Splits a query string into decoded pairs, refusing a parameter not in
/// `allowed` — the API's rule that a typo fails loudly.
fn query_pairs(raw: &str, allowed: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown query parameter: {key}"));
        }
        pairs.push((key, percent_decode(value)));
    }
    Ok(pairs)
}

fn now_ns() -> u64 {
    traza::mcp::unix_nanos_now()
}

/// The tenant a request reads under: its `tenant=` selection bound by its
/// credential, or `Err` (answered 403) when a bound credential names another.
fn effective_tenant(
    selected: Option<String>,
    principal: Option<&traza::auth::Principal>,
) -> Result<Option<String>, ()> {
    bind_tenant(selected, principal)
}

// -------------------------------------------------------------------- tapes

fn tape(
    responder: &mut Responder<'_>,
    run: &str,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match query_pairs(query, &["tenant", "resolve", "max_steps"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let mut selected = None;
    let mut options = TapeOptions::default();
    for (key, value) in pairs {
        match key.as_str() {
            "tenant" => selected = Some(value),
            "resolve" => match value.as_str() {
                "true" | "1" => options.resolve = Some(tape_max_bytes()),
                "false" | "0" => options.resolve = None,
                _ => return responder.json(400, json!({"error": "resolve must be true or false"})),
            },
            _ => match value.parse::<usize>() {
                Ok(steps) if steps > 0 => options.max_steps = steps.min(MAX_TAPE_SPANS),
                _ => {
                    return responder.json(
                        400,
                        json!({"error": "max_steps must be a positive integer"}),
                    )
                }
            },
        }
    }
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.tape(effective.as_deref(), run, options) {
        Ok(Some(tape)) => responder.json(
            200,
            serde_json::to_value(tape).unwrap_or_else(|_| json!({})),
        ),
        Ok(None) => responder.json(404, json!({"error": "run not found"})),
        Err(error) => engine_error(responder, metrics, error),
    }
}

// --------------------------------------------------------------------- diff

fn diff(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match query_pairs(query, &["a", "b", "tenant"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let (mut a, mut b, mut selected) = (None, None, None);
    for (key, value) in pairs {
        match key.as_str() {
            "a" => a = Some(value),
            "b" => b = Some(value),
            _ => selected = Some(value),
        }
    }
    let (Some(a), Some(b)) = (a, b) else {
        return responder.json(
            400,
            json!({"error": "both runs are required: /v1/diff?a=<run>&b=<run>"}),
        );
    };
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.diff_runs(effective.as_deref(), &a, &b, now_ns()) {
        Ok(Ok(diff)) => responder.json(
            200,
            serde_json::to_value(diff).unwrap_or_else(|_| json!({})),
        ),
        Ok(Err(missing)) => responder.json(
            404,
            json!({"error": format!("run not found: {}", missing.0)}),
        ),
        Err(error) => engine_error(responder, metrics, error),
    }
}

// ------------------------------------------------------------------- recall

fn recall(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match query_pairs(query, &["q", "outcome", "limit", "tenant"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let mut words = None;
    let mut outcome = OutcomeFilter::Any;
    let mut limit = traza::recall::DEFAULT_RECALL_LIMIT;
    let mut selected = None;
    for (key, value) in pairs {
        match key.as_str() {
            "q" => words = Some(value),
            "outcome" => match OutcomeFilter::parse(&value) {
                Some(parsed) => outcome = parsed,
                None => {
                    return responder.json(
                        400,
                        json!({"error": "outcome must be success, failure or any"}),
                    )
                }
            },
            "limit" => match value.parse::<usize>() {
                Ok(parsed) if parsed > 0 => limit = parsed.min(traza::recall::MAX_RECALL_LIMIT),
                _ => {
                    return responder
                        .json(400, json!({"error": "limit must be a positive integer"}))
                }
            },
            _ => selected = Some(value),
        }
    }
    let Some(words) = words.filter(|words| !words.trim().is_empty()) else {
        return responder.json(
            400,
            json!({"error": "q is required: the words to recall runs by"}),
        );
    };
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.recall(effective.as_deref(), &words, outcome, limit, now_ns()) {
        Ok(recall) => responder.json(
            200,
            serde_json::to_value(recall).unwrap_or_else(|_| json!({})),
        ),
        Err(error) => engine_error(responder, metrics, error),
    }
}

// ------------------------------------------------------------------ replays

fn create(
    responder: &mut Responder<'_>,
    body: &[u8],
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let request: Map<String, Value> = match serde_json::from_slice(body) {
        Ok(Value::Object(object)) => object,
        Ok(_) => {
            return responder.json(
                400,
                json!({"error": "body must be {\"run\": \"<run id>\", \"tenant\"?: \"..\", \"mode\"?: \"strict\"|\"loose\"}"}),
            )
        }
        Err(error) => return responder.json(400, json!({"error": error.to_string()})),
    };
    if let Some(unknown) = request
        .keys()
        .find(|key| !matches!(key.as_str(), "run" | "tenant" | "mode"))
    {
        return responder.json(400, json!({"error": format!("unknown field: {unknown}")}));
    }
    let Some(run) = request
        .get("run")
        .and_then(Value::as_str)
        .filter(|run| !run.is_empty())
    else {
        return responder.json(
            400,
            json!({"error": "run is required and must be a non-empty string"}),
        );
    };
    let selected = match request.get("tenant") {
        None | Some(Value::Null) => None,
        Some(Value::String(tenant)) => Some(tenant.clone()),
        Some(_) => return responder.json(400, json!({"error": "tenant must be a string"})),
    };
    let mode = match request.get("mode") {
        None | Some(Value::Null) => Mode::Strict,
        Some(Value::String(name)) => match Mode::parse(name) {
            Some(mode) => mode,
            None => return responder.json(400, json!({"error": "mode must be strict or loose"})),
        },
        Some(_) => return responder.json(400, json!({"error": "mode must be strict or loose"})),
    };
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match registry().create(
        engine,
        effective.as_deref(),
        run,
        mode,
        tape_max_bytes(),
        now_ns(),
    ) {
        Ok(created) => send_json(responder, 201, &[], &created.to_json()),
        Err(ReplayError::RunNotFound) => responder.json(404, json!({"error": "run not found"})),
        Err(ReplayError::Unreplayable(reason)) => {
            send_json(responder, 409, &[], &json!({"error": reason}))
        }
        Err(ReplayError::Full {
            retry_after_seconds,
        }) => send_json(
            responder,
            429,
            &[("Retry-After", retry_after_seconds.to_string())],
            &json!({"error": format!(
                "the replay registry is full ({} live replays, or {} MiB of recorded content); \
                 retry once the oldest expires",
                traza::replay::MAX_LIVE_REPLAYS,
                traza::replay::MAX_REGISTRY_BYTES / (1024 * 1024),
            )}),
        ),
        Err(ReplayError::Engine(error)) => engine_error(responder, metrics, error),
        Err(other) => responder.json(503, json!({"error": format!("{other:?}")})),
    }
}

/// Which provider's shape an error is answered in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    OpenAi,
    Anthropic,
    Native,
}

/// A replay error in the caller's shape. Divergence and exhaustion carry
/// `x-should-retry: false` (see the module docs).
fn replay_error(responder: &mut Responder<'_>, shape: Shape, error: ReplayError) -> io::Result<()> {
    let no_retry = [("x-should-retry", "false".to_owned())];
    let (status, kind, message, mut extra, retry) = match error {
        ReplayError::NotFound => (
            404,
            "not_found_error",
            "no live replay has this id; replays expire an hour after creation and do not \
             survive a restart"
                .to_owned(),
            Map::new(),
            false,
        ),
        ReplayError::Diverged {
            step,
            expected_digest,
            got_digest,
        } => {
            let mut extra = Map::new();
            extra.insert("step".to_owned(), json!(step));
            extra.insert("expected_digest".to_owned(), json!(expected_digest));
            extra.insert("got_digest".to_owned(), json!(got_digest));
            (
                409,
                "replay_divergence",
                format!(
                    "this request's messages do not match recorded step {step}: the agent \
                     diverged from the recording, so replaying the recorded answer would \
                     answer a different question"
                ),
                extra,
                true,
            )
        }
        ReplayError::Exhausted { served } => {
            let mut extra = Map::new();
            extra.insert("served".to_owned(), json!(served));
            (
                410,
                "replay_exhausted",
                format!("all {served} recorded model calls have been served"),
                extra,
                true,
            )
        }
        ReplayError::ToolsExhausted { tool } => (
            410,
            "replay_exhausted",
            match tool {
                Some(name) => format!("no unserved recorded tool step named {name} remains"),
                None => "no unserved recorded tool step remains".to_owned(),
            },
            Map::new(),
            true,
        ),
        ReplayError::Invalid(reason) => (400, "invalid_request_error", reason, Map::new(), false),
        other => (
            503,
            "api_error",
            format!("the replay could not be served: {other:?}"),
            Map::new(),
            false,
        ),
    };
    let headers: &[(&str, String)] = if retry { &no_retry } else { &[] };
    if shape == Shape::Native && status == 404 {
        return send_json(
            responder,
            status,
            headers,
            &json!({"error": "replay not found"}),
        );
    }
    extra.insert("type".to_owned(), json!(kind));
    extra.insert("message".to_owned(), json!(message));
    let mut body = Map::new();
    if shape == Shape::Anthropic {
        body.insert("type".to_owned(), json!("error"));
    }
    body.insert("error".to_owned(), Value::Object(extra));
    send_json(responder, status, headers, &Value::Object(body))
}

/// A route under `/v1/replays/{id}`.
fn call(
    responder: &mut Responder<'_>,
    method: &str,
    id: &str,
    route: &str,
    body: &[u8],
    binding: Option<&str>,
) -> io::Result<()> {
    let now = now_ns();
    match (method, route) {
        ("GET", "") => match registry().status(id, binding, now) {
            Ok(status) => responder.json(200, status),
            Err(error) => replay_error(responder, Shape::Native, error),
        },
        ("POST", OPENAI_CHAT) | ("POST", ANTHROPIC_MESSAGES) => {
            let shape = if route == OPENAI_CHAT {
                Shape::OpenAi
            } else {
                Shape::Anthropic
            };
            let request: Value = match serde_json::from_slice(body) {
                Ok(request @ Value::Object(_)) => request,
                Ok(_) => {
                    return replay_error(
                        responder,
                        shape,
                        ReplayError::Invalid("the request body must be a JSON object".to_owned()),
                    )
                }
                Err(error) => {
                    return replay_error(responder, shape, ReplayError::Invalid(error.to_string()))
                }
            };
            let digest = match shape {
                Shape::OpenAi => traza::replay::openai_request_digest(&request),
                _ => traza::replay::anthropic_request_digest(&request),
            };
            let digest = match digest {
                Ok(digest) => digest,
                Err(reason) => return replay_error(responder, shape, ReplayError::Invalid(reason)),
            };
            match registry().next_llm(id, binding, digest, now) {
                Ok(served) => {
                    let rendered = match shape {
                        Shape::OpenAi => served.openai(&request),
                        _ => served.anthropic(&request),
                    };
                    send(responder, 200, rendered.content_type, &[], &rendered.body)
                }
                Err(error) => replay_error(responder, shape, error),
            }
        }
        ("POST", TOOLS_NEXT) => {
            let tool = if body.iter().all(u8::is_ascii_whitespace) {
                None
            } else {
                match serde_json::from_slice::<Value>(body) {
                    Ok(Value::Object(object)) => {
                        if let Some(unknown) = object.keys().find(|key| key.as_str() != "tool") {
                            return responder
                                .json(400, json!({"error": format!("unknown field: {unknown}")}));
                        }
                        match object.get("tool") {
                            None | Some(Value::Null) => None,
                            Some(Value::String(name)) => Some(name.clone()),
                            Some(_) => {
                                return responder
                                    .json(400, json!({"error": "tool must be a string"}))
                            }
                        }
                    }
                    Ok(_) => {
                        return responder.json(
                            400,
                            json!({"error": "body must be {\"tool\"?: \"<name>\"}"}),
                        )
                    }
                    Err(error) => return responder.json(400, json!({"error": error.to_string()})),
                }
            };
            match registry().next_tool(id, binding, tool.as_deref(), now) {
                Ok(served) => responder.json(200, served.to_json()),
                Err(error) => replay_error(responder, Shape::Native, error),
            }
        }
        (_, route) if route.starts_with("/openai/") => replay_error(
            responder,
            Shape::OpenAi,
            ReplayError::Invalid(format!(
                "a replay serves POST {OPENAI_CHAT} only; {method} {route} is not replayed"
            )),
        ),
        (_, route) if route.starts_with("/anthropic/") => replay_error(
            responder,
            Shape::Anthropic,
            ReplayError::Invalid(format!(
                "a replay serves POST {ANTHROPIC_MESSAGES} only; {method} {route} is not replayed"
            )),
        ),
        _ => responder.json(404, json!({"error": "not found"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_api_key_stands_in_for_a_bearer_only_on_replay_call_routes() {
        let call = "/v1/replays/rp_1/anthropic/v1/messages";
        assert_eq!(
            presented_credential(None, Some("tok"), call).as_deref(),
            Some("Bearer tok")
        );
        // Authorization wins when both are sent.
        assert_eq!(
            presented_credential(Some("Bearer other"), Some("tok"), call).as_deref(),
            Some("Bearer other")
        );
        for elsewhere in ["/v1/spans", "/v1/replays", "/v1/tapes/x", "/v1/mcp"] {
            assert_eq!(presented_credential(None, Some("tok"), elsewhere), None);
        }
    }

    #[test]
    fn the_module_claims_exactly_its_routes() {
        for path in [
            "/v1/replays",
            "/v1/replays/rp_1",
            "/v1/tapes/trace:abc",
            "/v1/diff",
            "/v1/recall",
        ] {
            assert!(handles(path), "{path}");
        }
        for path in ["/v1/replay", "/v1/tapes", "/v1/diffs", "/v1/spans"] {
            assert!(!handles(path), "{path}");
        }
    }
}
