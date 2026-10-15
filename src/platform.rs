//! The hosting surfaces: health, identity, runtime credentials, tenant limits
//! and usage — what a control plane needs from the engine to run it for other
//! people.
//!
//! The state lives in the store, beside the data it governs, in three files
//! that are deliberately OUTSIDE generations and pins: `credentials.jsonl`
//! ([`crate::credentials`]), `limits.json` ([`crate::limits`]) and
//! `usage.json` ([`crate::usage`]). A backup carries telemetry, not secrets,
//! commercial terms or billing counters, and a restore winds back none of
//! them.
//!
//! The HTTP handlers live here rather than in the server binary so the server
//! routes to them with one call, and so their status codes and bodies are
//! testable without a socket. They are pure request → [`Reply`] functions:
//! authentication and the capability gate have already run by the time
//! [`route`] is called, and the admin handlers re-check the one rule that
//! matters most — unbound admin only — rather than trusting that they were
//! reached through the gate.

use std::path::Path;

use serde_json::{json, Value};

use crate::auth::{Principal, Required, Scope};
use crate::credentials::{CredentialStore, NewCredential, Revoked};
use crate::limits::{LimitsBook, TenantLimits};
use crate::usage::UsageBook;
use crate::{Error, Result, Store};

/// How the default tenant is spelled in a URL path segment, where the empty
/// string cannot be.
pub const DEFAULT_TENANT_SEGMENT: &str = "_default";

/// The per-store hosting state.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) credentials: CredentialStore,
    pub(crate) limits: LimitsBook,
    pub(crate) usage: UsageBook,
}

impl State {
    /// Opens all three files in `directory`. Each refuses a damaged file
    /// rather than starting empty; see their modules for why.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        Ok(Self {
            credentials: CredentialStore::open(directory)?,
            limits: LimitsBook::open(directory)?,
            usage: UsageBook::open(directory)?,
        })
    }
}

impl Store {
    /// The credential set requests authenticate against: `TRAZA_TOKENS`'s
    /// entries once installed, plus every runtime credential.
    pub fn credentials(&self) -> &CredentialStore {
        &self.platform.credentials
    }
}

/// A handler's answer: a status and a JSON body.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The JSON body.
    pub body: Value,
}

impl Reply {
    fn new(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    fn error(status: u16, message: impl Into<String>) -> Self {
        Self::new(status, json!({"error": message.into()}))
    }

    fn forbidden(required: Required, message: &str) -> Self {
        Self::new(
            403,
            json!({"error": message, "required": required.as_str()}),
        )
    }
}

/// What [`screen`] reads from a request: its head, and how the gate classified
/// it. Everything here is known before the body is read.
#[derive(Clone, Copy, Debug)]
pub struct Head<'a> {
    /// The request method, as sent.
    pub method: &'a str,
    /// The path, without its query string.
    pub path: &'a str,
    /// Whether the route needs the `admin` capability: erasure, eval
    /// tombstones, backups, ledger anchors, and everything under
    /// `/v1/admin/`.
    pub needs_admin: bool,
    /// Whether any credential exists, so requests authenticate. `false` is
    /// open mode.
    pub enforced: bool,
    /// `Origin:` as sent (lowercased), if any.
    pub origin: Option<&'a str>,
    /// `Content-Type:` as sent (lowercased), empty when absent.
    pub content_type: &'a str,
    /// The declared body length.
    pub content_length: usize,
}

/// Refusals the server decides from a request's head once authentication has
/// passed, before reading the body. `None` lets the request through.
///
/// **Cross-site requests in open mode.** With no credential configured, the
/// only thing standing between a web page the operator happens to visit and
/// the admin surfaces of a loopback server is the browser — and a browser
/// sends a cross-site `POST` with a `text/plain` body without asking anyone
/// (a "simple" request, no CORS preflight). Such a page could mint an admin
/// credential with a token it chose: the operator is locked out on the spot,
/// the attacker holds a live admin token, and the credential log then counts
/// as configured auth, so a later non-loopback start serves that token to the
/// network. Erasure, tombstones, backups and ledger anchors are the same
/// request shape — an anchor written cross-site would plant a notary
/// countersignature nobody gave into the chain an auditor relies on. So in
/// open mode every admin-capability route refuses a request whose `Origin`
/// is not a loopback page. A browser stamps `Origin` on every cross-origin
/// `POST` and a page cannot forge it; `curl` and scripts send none and are
/// unaffected. Once a credential exists the rule is moot — a page cannot
/// attach a bearer token it does not know — and it is not applied.
///
/// **JSON or nothing on `/v1/admin/*`.** Every admin `POST` that carries a
/// body must declare `Content-Type: application/json`. An HTML form can only
/// send `application/x-www-form-urlencoded`, `multipart/form-data` or
/// `text/plain`, and a cross-site `fetch` declaring JSON needs a CORS
/// preflight this server never approves — so this closes the form-POST route
/// even in a browser that omits `Origin`. It applies whether or not auth is
/// enforced, so a client that works before the first credential keeps working
/// after it.
pub fn screen(head: &Head<'_>) -> Option<Reply> {
    if head.needs_admin && !head.enforced {
        if let Some(origin) = head.origin {
            if !loopback_origin(origin) {
                return Some(Reply::new(
                    403,
                    json!({
                        "error": "origin not allowed: with no credential configured, admin \
                                  operations accept browser requests from loopback pages only",
                        "next": "send the request without an Origin header (curl, a script), \
                                 or create a credential and authenticate with it",
                    }),
                ));
            }
        }
    }
    let admin_surface = head.path == "/v1/admin" || head.path.starts_with("/v1/admin/");
    if admin_surface && head.method.eq_ignore_ascii_case("POST") && head.content_length > 0 {
        let media_type = head.content_type.split(';').next().unwrap_or("").trim();
        if media_type != "application/json" {
            return Some(Reply::error(
                415,
                "admin requests with a body must send Content-Type: application/json",
            ));
        }
    }
    None
}

/// Whether a browser `Origin` is a loopback page: `http` or `https` on
/// `localhost`, `127.0.0.1` or `::1`, any port.
///
/// This is a statement about the PAGE, not about where the request is going.
/// A page served from `evil.example` has that origin however its DNS
/// resolves — which is what makes this a defence against DNS rebinding as
/// well as against cross-site requests. The origin is deliberately never
/// compared with the request's `Host`: a rebinding request supplies the
/// attacker's name in both, so they agree exactly when it matters. `null`
/// (a sandboxed iframe or a `file://` page) is same-origin with nothing and
/// is not loopback.
pub fn loopback_origin(origin: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    // The scheme is checked too, so `javascript://localhost` and friends
    // cannot pass as one.
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal, unwrapped from its brackets so `[::1]:8080`
        // compares as `::1`.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.split(':').next().unwrap_or(authority),
    };
    matches!(scheme, "http" | "https")
        && matches!(
            host.to_ascii_lowercase().as_str(),
            "localhost" | "127.0.0.1" | "::1"
        )
}

/// `GET /healthz`: the process is up. Answered before authentication.
pub fn health() -> Value {
    json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
}

/// `GET /readyz`: the store is open and serving. The server binds its
/// listener only after the store has opened and recovered, so a process that
/// can answer at all is ready; "not yet" is a refused connection.
pub fn readiness() -> Value {
    json!({"ready": true})
}

/// Routes one authenticated request to its hosting handler, or `None` when
/// the path is not one of them.
///
/// `principal` is `None` in open mode (no credential configured anywhere),
/// which is unbound admin by definition.
pub fn route(
    engine: &Store,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    principal: Option<&Principal>,
) -> Option<Reply> {
    let now_ns = crate::unix_now_ns();
    match (method, path) {
        ("GET", "/v1/whoami") => Some(whoami(principal)),
        ("GET", "/v1/usage") => Some(usage(engine, query, principal)),
        (_, path) if path.starts_with("/v1/admin/") => {
            if let Some(refusal) = require_unbound_admin(principal) {
                return Some(refusal);
            }
            admin(engine, method, &path["/v1/admin/".len()..], body, now_ns)
        }
        _ => None,
    }
}

/// `GET /v1/whoami`.
fn whoami(principal: Option<&Principal>) -> Reply {
    Reply::new(
        200,
        match principal {
            None => json!({
                "auth": "open",
                "scope": Scope::Admin.as_str(),
                "tenant": null,
                "operator": true,
                "credential": null,
            }),
            Some(principal) => json!({
                "auth": "token",
                "scope": principal.scope.as_str(),
                "tenant": principal.tenant,
                "operator": principal.is_operator(),
                "credential": principal.credential.as_ref().map(|credential| json!({
                    "id": credential.id,
                    "label": credential.label,
                })),
            }),
        },
    )
}

/// `GET /v1/usage[?tenant=]`.
fn usage(engine: &Store, query: &str, principal: Option<&Principal>) -> Reply {
    let mut selected: Option<String> = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "tenant" => selected = Some(percent_decode(value)),
            other => return Reply::error(400, format!("unknown query parameter: {other}")),
        }
    }
    let bound = principal.and_then(|principal| principal.tenant.clone());
    let tenant = match (bound, selected) {
        (Some(bound), Some(named)) if named != bound => {
            return Reply::forbidden(
                Required::Unbound,
                "this credential is bound to a different tenant",
            )
        }
        (Some(bound), _) => Some(bound),
        (None, selected) => selected,
    };
    match engine.usage_report(tenant.as_deref()) {
        Ok(report) => Reply::new(
            200,
            serde_json::to_value(report).unwrap_or_else(|_| json!({})),
        ),
        Err(error) => error_reply(error),
    }
}

/// The admin surfaces, `rest` being the path after `/v1/admin/`.
fn admin(engine: &Store, method: &str, rest: &str, body: &[u8], now_ns: u64) -> Option<Reply> {
    let segments: Vec<&str> = rest.split('/').collect();
    Some(match (method, segments.as_slice()) {
        ("POST", ["credentials"]) => {
            let request: NewCredential = match serde_json::from_slice(body) {
                Ok(request) => request,
                Err(error) => return Some(Reply::error(400, error.to_string())),
            };
            match engine.credentials().create(request, now_ns) {
                Ok(created) => Reply::new(201, created.to_json()),
                Err(error) => error_reply(error),
            }
        }
        ("GET", ["credentials"]) => match engine.credentials().list() {
            Ok(views) => Reply::new(200, json!({"credentials": views})),
            Err(error) => error_reply(error),
        },
        ("POST", ["credentials", id, "revoke"]) => match engine.credentials().revoke(id, now_ns) {
            Ok(Revoked::At(at)) => Reply::new(200, json!({"id": id, "revoked_at_ns": at})),
            Ok(Revoked::Unknown) => Reply::error(404, format!("no credential with id {id}")),
            Err(error) => error_reply(error),
        },
        (_, ["tenants", segment, "limits"]) => {
            let tenant = match tenant_segment(segment) {
                Ok(tenant) => tenant,
                Err(reply) => return Some(reply),
            };
            match method {
                "GET" => match engine.tenant_limits(&tenant) {
                    Ok(limits) => Reply::new(200, limits_doc(&tenant, &limits)),
                    Err(error) => error_reply(error),
                },
                "POST" => {
                    let limits: TenantLimits = match serde_json::from_slice(body) {
                        Ok(limits) => limits,
                        Err(error) => return Some(Reply::error(400, error.to_string())),
                    };
                    match engine.set_tenant_limits(&tenant, limits) {
                        Ok(stored) => Reply::new(200, limits_doc(&tenant, &stored)),
                        Err(error) => error_reply(error),
                    }
                }
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// The limits document plus its `tenant`.
fn limits_doc(tenant: &str, limits: &TenantLimits) -> Value {
    let mut doc = serde_json::to_value(limits).unwrap_or_else(|_| json!({}));
    if let Some(object) = doc.as_object_mut() {
        object.insert("tenant".to_owned(), Value::String(tenant.to_owned()));
    }
    doc
}

/// A tenant named in a path segment: `_default` is the default tenant, and
/// anything else must be a tenant a span could carry.
fn tenant_segment(segment: &str) -> std::result::Result<String, Reply> {
    if segment == DEFAULT_TENANT_SEGMENT {
        return Ok(String::new());
    }
    if crate::valid_tenant(segment) {
        return Ok(segment.to_owned());
    }
    Err(Reply::error(
        400,
        format!(
            "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes, or \
             {DEFAULT_TENANT_SEGMENT} for the default tenant"
        ),
    ))
}

/// The admin rule, checked again here: unbound and admin, or open mode.
fn require_unbound_admin(principal: Option<&Principal>) -> Option<Reply> {
    let principal = principal?;
    if !principal.is_operator() {
        return Some(Reply::forbidden(Required::Unbound, "forbidden"));
    }
    if principal.scope != Scope::Admin {
        return Some(Reply::forbidden(Required::Admin, "forbidden"));
    }
    None
}

fn error_reply(error: Error) -> Reply {
    match error {
        Error::Invalid(reason) => Reply::error(400, reason),
        Error::Conflict(reason) => Reply::error(409, reason),
        other => Reply::error(503, other.to_string()),
    }
}

/// `%XX` and `+` decoding for a query value.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                output.push(high * 16 + low);
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
    use crate::auth::CredentialRef;

    fn principal(scope: Scope, tenant: Option<&str>) -> Principal {
        Principal {
            scope,
            tenant: tenant.map(str::to_owned),
            credential: Some(CredentialRef {
                id: "cr_0123456789abcdef".into(),
                label: "ops".into(),
            }),
        }
    }

    #[test]
    fn whoami_describes_open_mode_and_each_credential() {
        assert_eq!(
            whoami(None).body,
            json!({"auth": "open", "scope": "admin", "tenant": null, "operator": true,
                   "credential": null})
        );
        assert_eq!(
            whoami(Some(&principal(Scope::ReadWrite, Some("acme")))).body,
            json!({"auth": "token", "scope": "rw", "tenant": "acme", "operator": false,
                   "credential": {"id": "cr_0123456789abcdef", "label": "ops"}})
        );
        assert_eq!(
            whoami(Some(&principal(Scope::ReadOnly, None))).body["operator"],
            json!(true),
            "unbound is what makes an operator, not admin"
        );
    }

    #[test]
    fn the_admin_rule_holds_without_the_gate() {
        assert_eq!(
            require_unbound_admin(None),
            None,
            "open mode is unbound admin"
        );
        assert_eq!(
            require_unbound_admin(Some(&principal(Scope::Admin, None))),
            None
        );
        let bound =
            require_unbound_admin(Some(&principal(Scope::Admin, Some("acme")))).expect("refused");
        assert_eq!(
            (bound.status, &bound.body["required"]),
            (403, &json!("unbound"))
        );
        let reader =
            require_unbound_admin(Some(&principal(Scope::ReadWrite, None))).expect("refused");
        assert_eq!(reader.body["required"], json!("admin"));
    }

    #[test]
    fn tenant_segments_spell_the_default_tenant_and_refuse_the_rest() {
        assert_eq!(tenant_segment("_default"), Ok(String::new()));
        assert_eq!(tenant_segment("acme"), Ok("acme".to_owned()));
        assert_eq!(
            tenant_segment("Acme").map_err(|reply| reply.status),
            Err(400)
        );
        assert_eq!(tenant_segment("").map_err(|reply| reply.status), Err(400));
    }

    fn head<'a>(path: &'a str, origin: Option<&'a str>, content_type: &'a str) -> Head<'a> {
        Head {
            method: "POST",
            path,
            needs_admin: true,
            enforced: false,
            origin,
            content_type,
            content_length: 16,
        }
    }

    #[test]
    fn open_mode_refuses_cross_site_admin_requests_and_only_those() {
        // The attack: a page on another site POSTs a credential as text/plain.
        let attack = screen(&head(
            "/v1/admin/credentials",
            Some("https://evil.example"),
            "text/plain",
        ))
        .expect("refused");
        assert_eq!(attack.status, 403);
        assert!(
            attack.body["error"]
                .as_str()
                .is_some_and(|error| error.starts_with("origin not allowed")),
            "{}",
            attack.body
        );
        // Erasure and the other admin-capability routes are the same shape.
        assert_eq!(
            screen(&head(
                "/v1/erasures",
                Some("http://evil.example:8080"),
                "text/plain"
            ))
            .map(|reply| reply.status),
            Some(403)
        );
        // A loopback page, or no browser at all, is let through.
        for origin in [
            None,
            Some("http://127.0.0.1:8080"),
            Some("http://localhost:5173"),
            Some("https://[::1]:8443"),
        ] {
            assert_eq!(
                screen(&head("/v1/admin/credentials", origin, "application/json")),
                None,
                "{origin:?}"
            );
        }
        // Once a credential exists, a page cannot attach the bearer it would
        // need, so the origin no longer matters.
        let enforced = Head {
            enforced: true,
            ..head(
                "/v1/admin/credentials",
                Some("https://evil.example"),
                "application/json",
            )
        };
        assert_eq!(screen(&enforced), None);
        // Routes that do not need admin are not this rule's business.
        let ingest = Head {
            needs_admin: false,
            ..head("/v1/spans", Some("https://evil.example"), "text/plain")
        };
        assert_eq!(screen(&ingest), None);
    }

    #[test]
    fn admin_bodies_must_be_declared_json() {
        for content_type in [
            "",
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/jsonp",
        ] {
            let enforced = Head {
                enforced: true,
                ..head("/v1/admin/credentials", None, content_type)
            };
            assert_eq!(
                screen(&enforced).map(|reply| reply.status),
                Some(415),
                "{content_type:?}"
            );
        }
        assert_eq!(
            screen(&head(
                "/v1/admin/tenants/acme/limits",
                None,
                "application/json; charset=utf-8"
            )),
            None
        );
        // A bodyless POST (a revoke) declares nothing and needs nothing; a
        // GET is not a write; and the rule is about /v1/admin/ only.
        let revoke = Head {
            content_length: 0,
            ..head("/v1/admin/credentials/cr_x/revoke", None, "")
        };
        assert_eq!(screen(&revoke), None);
        let list = Head {
            method: "GET",
            ..head("/v1/admin/credentials", None, "")
        };
        assert_eq!(screen(&list), None);
        assert_eq!(screen(&head("/v1/erasures", None, "text/plain")), None);
    }

    #[test]
    fn loopback_origins_are_pages_on_loopback_and_nothing_else() {
        for origin in [
            "http://localhost",
            "http://localhost:8080",
            "https://127.0.0.1:8443",
            "http://[::1]:8080",
            "http://LOCALHOST:8080",
            "http://127.0.0.1:8080/",
        ] {
            assert!(loopback_origin(origin), "{origin}");
        }
        for origin in [
            "",
            "null",
            "https://evil.example",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1@evil.example",
            "javascript://localhost",
            "file://localhost",
            "localhost:8080",
            "http://[::2]:8080",
        ] {
            assert!(!loopback_origin(origin), "{origin}");
        }
    }

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(percent_decode("a%2Eb"), "a.b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("tail%2"), "tail%2");
    }
}
