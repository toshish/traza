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
//! - `GET /v1/forks?run=&limit=&tenant=` — a run's forks, each diffed against
//!   it ([`traza::fork`]).
//! - `GET /v1/goldens?limit=&tenant=` — the runs marked as regression
//!   baselines.
//!
//! The provider-shaped routes answer errors in the provider's shape —
//! `{"error": {"type", "message"}}`, plus `"type": "error"` at the top for
//! Anthropic — because the client reading them is that provider's SDK; the
//! native routes answer the API's `{"error": "<message>"}` with the kind
//! beside it as `"type"`. Every refusal a retry cannot cure — divergence,
//! exhaustion, erasure, an unreplayable run, an answer too large to render —
//! is marked `x-should-retry: false`: both SDKs retry a 409 by default, and
//! retrying a request that does not match the recording cannot make it match.

use std::io;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};
use traza::fork::{ForkSpec, DEFAULT_FORK_LIMIT, DEFAULT_GOLDEN_LIMIT};
use traza::recall::OutcomeFilter;
use traza::replay::{Arrival, Mode, Provider, Registry, ReplayError, ServedStep};
use traza::tape::{TapeOptions, DEFAULT_TAPE_MAX_BYTES, MAX_TAPE_SPANS};
use traza::Store;

use super::{bind_tenant, engine_error, foreign_tenant, percent_decode, Responder, ServerMetrics};

const REPLAYS: &str = "/v1/replays";
const REPLAY_PREFIX: &str = "/v1/replays/";
const TAPES_PREFIX: &str = "/v1/tapes/";
const OPENAI_CHAT: &str = "/openai/v1/chat/completions";
const ANTHROPIC_MESSAGES: &str = "/anthropic/v1/messages";
const TOOLS_NEXT: &str = "/tools/next";

/// `Retry-After` for a bound caller refused because the WHOLE registry is
/// full (see [`create`]): a minute, a fixed figure that says nothing about
/// when another tenant's replay expires.
const REGISTRY_FULL_RETRY_SECONDS: u64 = 60;

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
        || path == "/v1/forks"
        || path == "/v1/goldens"
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

/// Serves one of this module's routes. `arrival` is when the request's head
/// was read: the registry judges a replay's expiry from it, so everything
/// the request costs before it gets there counts.
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
    arrival: Arrival,
) -> io::Result<()> {
    let binding = principal.and_then(|principal| principal.tenant.as_deref());
    match (method, path) {
        ("GET", _) if path.starts_with(TAPES_PREFIX) => {
            let run = percent_decode(&path[TAPES_PREFIX.len()..]);
            tape(responder, &run, query, engine, metrics, principal)
        }
        ("GET", "/v1/diff") => diff(responder, query, engine, metrics, principal),
        ("GET", "/v1/recall") => recall(responder, query, engine, metrics, principal),
        ("GET", "/v1/forks") => forks(responder, query, engine, metrics, principal),
        ("GET", "/v1/goldens") => goldens(responder, query, engine, metrics, principal),
        ("POST", REPLAYS) => create(responder, body, engine, metrics, principal, arrival),
        (_, _) if path.starts_with(REPLAY_PREFIX) => {
            let rest = &path[REPLAY_PREFIX.len()..];
            let (id, route) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
            call(responder, method, id, route, body, engine, binding, arrival)
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
        // The caller's to narrow, as on span search: retrying cannot help.
        Err(error @ traza::Error::QueryTooBroad(_)) => {
            responder.json(400, json!({"error": error.to_string()}))
        }
        Err(error) => engine_error(responder, metrics, error),
    }
}

// -------------------------------------------------------- forks and goldens

/// A `limit` query value: a positive integer, clamped by the engine.
fn positive(value: &str) -> Option<usize> {
    value.parse::<usize>().ok().filter(|limit| *limit > 0)
}

fn forks(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match query_pairs(query, &["run", "limit", "tenant"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let (mut run, mut limit, mut selected) = (None, DEFAULT_FORK_LIMIT, None);
    for (key, value) in pairs {
        match key.as_str() {
            "run" => run = Some(value),
            "limit" => match positive(&value) {
                Some(parsed) => limit = parsed,
                None => {
                    return responder
                        .json(400, json!({"error": "limit must be a positive integer"}))
                }
            },
            _ => selected = Some(value),
        }
    }
    let Some(run) = run.filter(|run| !run.is_empty()) else {
        return responder.json(
            400,
            json!({"error": "run is required: /v1/forks?run=<run>"}),
        );
    };
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.forks(effective.as_deref(), &run, limit, now_ns()) {
        Ok(Some(forks)) => responder.json(
            200,
            serde_json::to_value(forks).unwrap_or_else(|_| json!({})),
        ),
        Ok(None) => responder.json(404, json!({"error": "run not found"})),
        Err(error) => engine_error(responder, metrics, error),
    }
}

fn goldens(
    responder: &mut Responder<'_>,
    query: &str,
    engine: &Store,
    metrics: &ServerMetrics,
    principal: Option<&traza::auth::Principal>,
) -> io::Result<()> {
    let pairs = match query_pairs(query, &["limit", "tenant"]) {
        Ok(pairs) => pairs,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let (mut limit, mut selected) = (DEFAULT_GOLDEN_LIMIT, None);
    for (key, value) in pairs {
        match key.as_str() {
            "limit" => match positive(&value) {
                Some(parsed) => limit = parsed,
                None => {
                    return responder
                        .json(400, json!({"error": "limit must be a positive integer"}))
                }
            },
            _ => selected = Some(value),
        }
    }
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match engine.goldens(effective.as_deref(), limit) {
        Ok(goldens) => responder.json(
            200,
            serde_json::to_value(goldens).unwrap_or_else(|_| json!({})),
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
    arrival: Arrival,
) -> io::Result<()> {
    let request: Map<String, Value> = match serde_json::from_slice(body) {
        Ok(Value::Object(object)) => object,
        Ok(_) => {
            return responder.json(
                400,
                json!({"error": "body must be {\"run\": \"<run id>\", \"tenant\"?: \"..\", \"mode\"?: \"strict\"|\"loose\", \"edits\"?: [..], \"live_after\"?: n}"}),
            )
        }
        Err(error) => return responder.json(400, json!({"error": error.to_string()})),
    };
    if let Some(unknown) = request.keys().find(|key| {
        !matches!(
            key.as_str(),
            "run" | "tenant" | "mode" | "edits" | "live_after"
        )
    }) {
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
    let fork = match ForkSpec::parse(request.get("edits"), request.get("live_after")) {
        Ok(fork) => fork,
        Err(error) => return responder.json(400, json!({"error": error})),
    };
    let effective = match effective_tenant(selected, principal) {
        Ok(tenant) => tenant,
        Err(()) => return foreign_tenant(responder),
    };
    match registry().create_fork(
        engine,
        effective.as_deref(),
        run,
        mode,
        &fork,
        tape_max_bytes(),
        arrival,
    ) {
        Ok(created) => send_json(responder, 201, &[], &created.to_json()),
        Err(ReplayError::RunNotFound) => responder.json(404, json!({"error": "run not found"})),
        // An edit or live point the run cannot take: the caller's to fix.
        Err(ReplayError::Invalid(reason)) => responder.json(400, json!({"error": reason})),
        // Permanent: the run cannot be replayed as asked, and a retry would
        // be refused the same way. `x-should-retry: false` for any SDK-style
        // client wrapping this call, which would otherwise retry a 409.
        Err(error @ (ReplayError::Unreplayable(_) | ReplayError::TooLarge { .. })) => send_json(
            responder,
            409,
            &[("x-should-retry", "false".to_owned())],
            &json!({"error": error.to_string()}),
        ),
        Err(
            error @ ReplayError::Full {
                own_share,
                retry_after_seconds,
                ..
            },
        ) => {
            // A bound caller refused by the registry as a whole is told to
            // come back later, not WHEN: the exact time would be when some
            // other tenant's replay expires, which is not its to learn. Its
            // own share's retry time is computed from its own replays.
            let bound = principal.is_some_and(|principal| principal.tenant.is_some());
            let retry_after = if own_share || !bound {
                retry_after_seconds
            } else {
                REGISTRY_FULL_RETRY_SECONDS
            };
            send_json(
                responder,
                429,
                &[("Retry-After", retry_after.to_string())],
                &json!({"error": error.to_string()}),
            )
        }
        Err(ReplayError::Engine(error)) => engine_error(responder, metrics, error),
        // Creation produces none of the call-time refusals; should one ever
        // arrive here it is still answered in words, never in debug output.
        Err(other) => responder.json(503, json!({"error": other.to_string()})),
    }
}

/// Which provider's shape an error is answered in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    OpenAi,
    Anthropic,
    Native,
}

/// A replay error in the caller's shape.
///
/// The provider routes answer `{"error": {"type", "message", …}}` (plus a
/// top-level `"type": "error"` for Anthropic), because the client reading
/// them is that provider's SDK. The native routes — status and `tools/next`
/// — answer the API's own convention, `{"error": "<message>"}`, with the
/// machine-readable kind beside it as `"type"` and any detail as further
/// siblings; an unknown replay there is exactly `{"error": "replay not
/// found"}`. Every refusal that a retry cannot cure — divergence, exhaustion,
/// erasure, an unreadable tool step, an answer too large to render — carries
/// `x-should-retry: false` (see the module docs).
fn replay_error(responder: &mut Responder<'_>, shape: Shape, error: ReplayError) -> io::Result<()> {
    replay_error_with(responder, shape, error, None)
}

/// [`replay_error`], carrying a divergence's first difference
/// ([`Registry::explain_divergence`]) beside its digests when there is one.
fn replay_error_with(
    responder: &mut Responder<'_>,
    shape: Shape,
    error: ReplayError,
    difference: Option<Value>,
) -> io::Result<()> {
    let message = error.to_string();
    let mut extra = Map::new();
    if let Some(difference) = difference {
        extra.insert("difference".to_owned(), difference);
    }
    let (status, kind, retry) = match error {
        ReplayError::NotFound => (404, "not_found_error", false),
        ReplayError::Diverged {
            step,
            expected_digest,
            got_digest,
        } => {
            extra.insert("step".to_owned(), json!(step));
            extra.insert("expected_digest".to_owned(), json!(expected_digest));
            extra.insert("got_digest".to_owned(), json!(got_digest));
            (409, "replay_divergence", true)
        }
        ReplayError::Exhausted { served } => {
            extra.insert("served".to_owned(), json!(served));
            (410, "replay_exhausted", true)
        }
        ReplayError::ToolsExhausted { .. } => (410, "replay_exhausted", true),
        ReplayError::ToolUnreadable { step, .. } => {
            extra.insert("step".to_owned(), json!(step));
            (409, "tool_unreadable", true)
        }
        ReplayError::ResponseTooLarge { step, limit } => {
            extra.insert("step".to_owned(), json!(step));
            extra.insert("limit".to_owned(), json!(limit));
            (409, "response_too_large", true)
        }
        ReplayError::Erased { erasure } => {
            extra.insert("erasure".to_owned(), json!(erasure));
            (410, "replay_erased", true)
        }
        // A fork's live point: the harness switches to the real provider.
        // Retrying the replay cannot serve what it never held.
        ReplayError::Live {
            live_after,
            served,
            fork_id,
        } => {
            extra.insert("live_after".to_owned(), json!(live_after));
            extra.insert("served".to_owned(), json!(served));
            extra.insert("fork_id".to_owned(), json!(fork_id));
            (409, "replay_live", true)
        }
        ReplayError::Invalid(_) => (400, "invalid_request_error", false),
        // Creation-time refusals never reach a call route; the store's own
        // failures (a poisoned lock, a failed read) land here, in words.
        ReplayError::Engine(_)
        | ReplayError::RunNotFound
        | ReplayError::Full { .. }
        | ReplayError::TooLarge { .. }
        | ReplayError::Unreplayable(_) => (503, "api_error", false),
    };
    let no_retry = [("x-should-retry", "false".to_owned())];
    let headers: &[(&str, String)] = if retry { &no_retry } else { &[] };
    if shape == Shape::Native {
        if status == 404 {
            return send_json(
                responder,
                status,
                headers,
                &json!({"error": "replay not found"}),
            );
        }
        extra.insert("error".to_owned(), json!(message));
        extra.insert("type".to_owned(), json!(kind));
        return send_json(responder, status, headers, &Value::Object(extra));
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

/// A route under `/v1/replays/{id}`. These routes take no query parameters:
/// an SDK appends none, and the replay's tenant was fixed when it was made.
///
/// Every registry call is handed the request's `arrival`, not a time taken
/// here: parsing and digesting a request body is work done after the request
/// arrived, and a call judged from after it would be served past its
/// replay's expiry by however long that took.
#[allow(clippy::too_many_arguments)]
fn call(
    responder: &mut Responder<'_>,
    method: &str,
    id: &str,
    route: &str,
    body: &[u8],
    engine: &Store,
    binding: Option<&str>,
    arrival: Arrival,
) -> io::Result<()> {
    match (method, route) {
        ("GET", "") => match registry().status(engine, id, binding, arrival) {
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
            let provider = match shape {
                Shape::OpenAi => Provider::OpenAi,
                _ => Provider::Anthropic,
            };
            let digest = provider.request_digest(&request);
            let digest = match digest {
                Ok(digest) => digest,
                Err(reason) => return replay_error(responder, shape, ReplayError::Invalid(reason)),
            };
            // Rendered before the step is consumed, so a rendering refused as
            // too large leaves the step for a request that asks differently.
            let render = |served: &ServedStep| match shape {
                Shape::OpenAi => served.openai(&request),
                _ => served.anthropic(&request),
            };
            match registry().next_llm(engine, id, binding, digest, arrival, render) {
                Ok(rendered) => send(responder, 200, rendered.content_type, &[], &rendered.body),
                // Where the request differs from the recording, read once the
                // refusal is decided and outside every replay lock.
                Err(error @ ReplayError::Diverged { step, .. }) => {
                    let difference = registry()
                        .explain_divergence(engine, id, binding, step, &request, provider, arrival);
                    replay_error_with(responder, shape, error, difference)
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
            match registry().next_tool(engine, id, binding, tool.as_deref(), arrival) {
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
            "/v1/forks",
            "/v1/goldens",
        ] {
            assert!(handles(path), "{path}");
        }
        for path in [
            "/v1/replay",
            "/v1/tapes",
            "/v1/diffs",
            "/v1/spans",
            "/v1/forks/x",
            "/v1/golden",
        ] {
            assert!(!handles(path), "{path}");
        }
    }
}
