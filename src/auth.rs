//! Bearer-token authentication for the HTTP server.
//!
//! Authentication configuration is intentionally kept out of logs and error
//! messages. `AuthConfig` also implements a redacted `Debug` representation so
//! an accidental structured log cannot disclose configured credentials.
//!
//! This module owns the vocabulary — [`Scope`], [`Principal`], the refusals —
//! and the `TRAZA_TOKENS` parser. The credential set a running server actually
//! authenticates against is [`crate::credentials::CredentialStore`]: the
//! environment's entries plus every credential minted at runtime, looked up by
//! digest.

use std::env;
use std::fmt;

/// Environment variable holding `token:scope` credentials (see README).
pub const AUTH_ENV: &str = "TRAZA_TOKENS";
/// JSON body returned with 401 responses.
pub const UNAUTHORIZED_BODY: &str = "{\"error\":\"unauthorized\"}";
/// JSON body returned with 403 responses that need the `rw` scope.
pub const FORBIDDEN_RW_BODY: &str = "{\"error\":\"forbidden\",\"required\":\"rw\"}";
/// JSON body returned with 403 responses that need the `admin` scope.
pub const FORBIDDEN_ADMIN_BODY: &str = "{\"error\":\"forbidden\",\"required\":\"admin\"}";
/// JSON body returned with 403 responses that need an unbound credential.
pub const FORBIDDEN_UNBOUND_BODY: &str = "{\"error\":\"forbidden\",\"required\":\"unbound\"}";
/// Challenge scheme advertised on 401 responses.
pub const WWW_AUTHENTICATE: &str = "Bearer";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// What a credential may do: read-only (GET), read-write (GET + POST), or
/// admin (read-write plus the destructive operations).
pub enum Scope {
    /// GET only.
    ReadOnly,
    /// GET and POST.
    ReadWrite,
    /// Everything `rw` permits, plus erasure. Deletion is not ingest: every
    /// collector and exporter holds an `rw` token, and a credential minted
    /// to WRITE telemetry must not be able to destroy it. `admin` exists so
    /// that the erase capability is a token an operator issued on purpose,
    /// not a side effect of being allowed to POST.
    Admin,
}

impl Scope {
    /// Parses the wire name: `ro`, `rw` or `admin`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ro" => Some(Self::ReadOnly),
            "rw" => Some(Self::ReadWrite),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// The wire name: `ro`, `rw` or `admin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
            Self::Admin => "admin",
        }
    }

    /// Whether this scope allows the given HTTP method.
    pub fn permits(self, method: &str) -> bool {
        match self {
            Self::ReadOnly => method.eq_ignore_ascii_case("GET"),
            Self::ReadWrite | Self::Admin => {
                method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("POST")
            }
        }
    }

    /// Whether this scope may erase data. The method rule cannot express
    /// this — erasure is a POST like any ingest — so the erasure route
    /// checks the capability explicitly.
    pub fn permits_erasure(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// What an authenticated credential is: what it may do, and — when bound —
/// whose data it may do it to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    /// The capability tier.
    pub scope: Scope,
    /// The tenant this credential is bound to, or `None` for an unbound
    /// (operator-level) credential. A bound principal reads and writes ONLY
    /// its tenant's data, on every surface; binding is applied where queries
    /// are constructed, so no endpoint can forget it.
    pub tenant: Option<String>,
    /// Which credential authenticated, when the credential set knows it —
    /// always, for a principal from [`crate::credentials::CredentialStore`];
    /// never, for one from [`AuthConfig::principal_for`], which predates
    /// credential ids. Identity only: nothing authorizes on it.
    pub credential: Option<CredentialRef>,
}

impl Principal {
    /// The tenant scope for store reads: `Some(t)` when bound, else `None`.
    pub fn tenant_scope(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Whether this principal may use the operator surfaces. Admin scope does
    /// not make an operator; the ABSENCE of a tenant binding does. A bound
    /// `admin` is its tenant's administrator and nobody else's.
    pub fn is_operator(&self) -> bool {
        self.tenant.is_none()
    }
}

/// The public handle of a credential: its id and its label, never anything
/// derived from the token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRef {
    /// `cr_` plus 16 hex digits.
    pub id: String,
    /// The operator's label; empty when none was given.
    pub label: String,
}

#[derive(Clone)]
struct Credential {
    token: Vec<u8>,
    scope: Scope,
    tenant: Option<String>,
}

#[derive(Clone)]
/// The parsed credential set; comparison is constant-time per token.
pub struct AuthConfig {
    credentials: Vec<Credential>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("credential_count", &self.credentials.len())
            .field("credentials", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A TRAZA_TOKENS value that could not be parsed.
pub struct ConfigError {
    message: &'static str,
}

impl ConfigError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ConfigError {}

/// The capability a refused request was missing, carried on every 403 as
/// `"required"` so a client can say WHICH grant would have worked instead of
/// only that this one did not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Required {
    /// The write scope: a `ro` credential attempted a `POST`.
    Rw,
    /// The `admin` scope: erasure, eval tombstones, backups, `/v1/admin/*`.
    Admin,
    /// An unbound credential: a tenant-bound one reached a store-global
    /// operator surface, `/v1/admin/*`, or another tenant's data.
    Unbound,
}

impl Required {
    /// The wire name: `rw`, `admin` or `unbound`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rw => "rw",
            Self::Admin => "admin",
            Self::Unbound => "unbound",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Why a request was refused: unknown token (401) or insufficient grant (403).
pub enum AuthFailure {
    /// No, malformed, unknown, expired or revoked bearer token (HTTP 401).
    Unauthorized,
    /// Valid token without the grant this request needs (HTTP 403), naming
    /// the capability that was missing.
    Forbidden(Required),
}

impl AuthFailure {
    /// The HTTP status for this failure (401 or 403).
    pub fn status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden(_) => 403,
        }
    }

    /// The JSON error body for this failure. Static per variant, so a refusal
    /// costs no allocation and can never echo request content.
    pub fn body(self) -> &'static str {
        match self {
            Self::Unauthorized => UNAUTHORIZED_BODY,
            Self::Forbidden(Required::Rw) => FORBIDDEN_RW_BODY,
            Self::Forbidden(Required::Admin) => FORBIDDEN_ADMIN_BODY,
            Self::Forbidden(Required::Unbound) => FORBIDDEN_UNBOUND_BODY,
        }
    }

    /// The WWW-Authenticate challenge, present on 401.
    pub fn www_authenticate(self) -> Option<&'static str> {
        match self {
            Self::Unauthorized => Some(WWW_AUTHENTICATE),
            Self::Forbidden(_) => None,
        }
    }
}

impl AuthConfig {
    /// Loads the mandatory server authentication configuration.
    ///
    /// The value is a comma-separated list of `scope:token` entries. Supported
    /// scopes are `ro` and `rw`; tokens must be nonempty and unique. Parsing
    /// errors deliberately identify only the configuration defect, never the
    /// offending value.
    /// Reads `TRAZA_TOKENS`; `Ok(None)` when unset. The server permits that
    /// mode on loopback by default and separately guards non-loopback binds.
    /// A SET but invalid value is a hard error: silently running open when the
    /// operator tried to configure auth would be the worst failure mode.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        match env::var(AUTH_ENV) {
            Err(_) => Ok(None),
            Ok(raw) => Self::parse(&raw).map(Some),
        }
    }

    /// Parses a TRAZA_TOKENS value (`scope[@tenant]:token`, comma-separated).
    ///
    /// The optional `@tenant` binds the credential: `rw@acme:tok` writes and
    /// reads tenant `acme` and nothing else. The split order is what makes
    /// the syntax unambiguous — the first `:` isolates the token (which may
    /// itself contain `@` or `:`), and only the left side is examined for a
    /// binding.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        if raw.is_empty() {
            return Err(ConfigError::new("TRAZA_TOKENS must not be empty"));
        }

        let mut credentials: Vec<Credential> = Vec::new();
        for entry in raw.split(',') {
            if entry.is_empty() || entry.trim() != entry {
                return Err(ConfigError::new("TRAZA_TOKENS contains an invalid entry"));
            }

            let (scope, token) = entry
                .split_once(':')
                .ok_or_else(|| ConfigError::new("TRAZA_TOKENS entries must use scope:token"))?;
            let (scope, tenant) = match scope.split_once('@') {
                None => (scope, None),
                Some((scope, tenant)) => {
                    if !crate::valid_tenant(tenant) {
                        return Err(ConfigError::new(
                            "TRAZA_TOKENS contains an invalid tenant binding",
                        ));
                    }
                    (scope, Some(tenant.to_owned()))
                }
            };
            let scope = Scope::parse(scope)
                .ok_or_else(|| ConfigError::new("TRAZA_TOKENS contains an invalid scope"))?;
            if token.is_empty() {
                return Err(ConfigError::new("TRAZA_TOKENS contains an empty token"));
            }
            if token
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte == b',')
            {
                return Err(ConfigError::new("TRAZA_TOKENS contains an invalid token"));
            }
            if credentials
                .iter()
                .any(|credential| constant_time_eq(&credential.token, token.as_bytes()))
            {
                return Err(ConfigError::new("TRAZA_TOKENS contains a duplicate token"));
            }
            credentials.push(Credential {
                token: token.as_bytes().to_vec(),
                scope,
                tenant,
            });
        }

        if credentials.is_empty() {
            return Err(ConfigError::new(
                "TRAZA_TOKENS must contain at least one credential",
            ));
        }
        Ok(Self { credentials })
    }

    /// Authenticates an HTTP Authorization header and enforces method scope.
    /// Checks a request: bearer token match (constant-time) + scope for the method.
    pub fn authorize(&self, authorization: Option<&str>, method: &str) -> Result<(), AuthFailure> {
        let principal = self.principal_for(authorization)?;
        if principal.scope.permits(method) {
            Ok(())
        } else {
            Err(AuthFailure::Forbidden(Required::Rw))
        }
    }

    /// Authenticates a bearer token and returns what it may do, without
    /// applying the HTTP-method rule.
    pub fn scope_for(&self, authorization: Option<&str>) -> Result<Scope, AuthFailure> {
        self.principal_for(authorization)
            .map(|principal| principal.scope)
    }

    /// Authenticates a bearer token and returns the full principal — scope
    /// AND tenant binding — without applying the HTTP-method rule.
    ///
    /// The method rule is right for the REST surface, where the method *is*
    /// the operation. It is wrong for a protocol that tunnels every operation
    /// through one `POST`: applied there it would lock `ro` tokens out of a
    /// read-only surface entirely, and hand every caller that got in the write
    /// scope. [`crate::mcp`] authorizes per tool instead, and this is the
    /// authentication half it builds on.
    pub fn principal_for(&self, authorization: Option<&str>) -> Result<Principal, AuthFailure> {
        let token = authorization
            .and_then(parse_bearer)
            .ok_or(AuthFailure::Unauthorized)?;

        // Check every configured credential even after a match. Besides avoiding
        // an early token-dependent return, this keeps lookup timing independent
        // of credential ordering. The tenant is metadata ON the matched
        // credential, never something compared against the request, so the
        // constant-time story is untouched.
        let mut matched: Option<Principal> = None;
        for credential in &self.credentials {
            if constant_time_eq(&credential.token, token.as_bytes()) {
                matched = Some(Principal {
                    scope: credential.scope,
                    tenant: credential.tenant.clone(),
                    credential: None,
                });
            }
        }
        matched.ok_or(AuthFailure::Unauthorized)
    }

    /// Every configured credential as `(token, scope, binding)`, in
    /// configuration order. Crate-private: the raw tokens leave this module
    /// only to be hashed by [`crate::credentials`], which keeps digests.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&[u8], Scope, Option<&str>)> {
        self.credentials.iter().map(|credential| {
            (
                credential.token.as_slice(),
                credential.scope,
                credential.tenant.as_deref(),
            )
        })
    }
}

/// The bearer token in an `Authorization` header value, or `None` when the
/// header is not exactly `Bearer <token>` with a token free of whitespace and
/// commas.
pub(crate) fn bearer_token(header: &str) -> Option<&str> {
    parse_bearer(header)
}

fn parse_bearer(header: &str) -> Option<&str> {
    let token = header.strip_prefix("Bearer ")?;
    if token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
        || token.contains(',')
    {
        return None;
    }
    Some(token)
}

/// Compares all bytes without returning early, including for unequal lengths.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_configured_scopes() {
        let auth = AuthConfig::parse("ro:reader-token,rw:writer-token").unwrap();
        assert_eq!(auth.authorize(Some("Bearer reader-token"), "GET"), Ok(()));
        assert_eq!(auth.authorize(Some("Bearer writer-token"), "POST"), Ok(()));
        assert_eq!(
            auth.authorize(Some("Bearer reader-token"), "POST"),
            Err(AuthFailure::Forbidden(Required::Rw))
        );
    }

    #[test]
    fn every_refusal_names_the_missing_capability() {
        for (required, name) in [
            (Required::Rw, "rw"),
            (Required::Admin, "admin"),
            (Required::Unbound, "unbound"),
        ] {
            let failure = AuthFailure::Forbidden(required);
            assert_eq!(failure.status(), 403);
            assert_eq!(failure.www_authenticate(), None);
            let body: serde_json::Value = serde_json::from_str(failure.body()).expect("json");
            assert_eq!(
                body,
                serde_json::json!({"error": "forbidden", "required": name})
            );
            assert_eq!(required.as_str(), name);
        }
        let body: serde_json::Value =
            serde_json::from_str(AuthFailure::Unauthorized.body()).expect("json");
        assert_eq!(body, serde_json::json!({"error": "unauthorized"}));
    }

    #[test]
    fn scopes_round_trip_their_wire_names() {
        for scope in [Scope::ReadOnly, Scope::ReadWrite, Scope::Admin] {
            assert_eq!(Scope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(Scope::parse("root"), None);
    }

    #[test]
    fn rejects_missing_malformed_and_unknown_credentials() {
        let auth = AuthConfig::parse("rw:correct-token").unwrap();
        assert_eq!(auth.authorize(None, "GET"), Err(AuthFailure::Unauthorized));
        assert_eq!(
            auth.authorize(Some("correct-token"), "GET"),
            Err(AuthFailure::Unauthorized)
        );
        assert_eq!(
            auth.authorize(Some("bearer correct-token"), "GET"),
            Err(AuthFailure::Unauthorized)
        );
        assert_eq!(
            auth.authorize(Some("Bearer incorrect-token"), "GET"),
            Err(AuthFailure::Unauthorized)
        );
    }

    #[test]
    fn configuration_errors_and_debug_do_not_disclose_tokens() {
        let secret = "do-not-print-this-secret";
        let auth = AuthConfig::parse(&format!("rw:{secret}")).unwrap();
        assert!(!format!("{auth:?}").contains(secret));

        let error = AuthConfig::parse(&format!("invalid:{secret}")).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn rejects_invalid_configuration() {
        assert!(AuthConfig::parse("").is_err());
        assert!(AuthConfig::parse("rw:").is_err());
        assert!(AuthConfig::parse("root:token").is_err());
        assert!(AuthConfig::parse("rw:token,rw:token").is_err());
        assert!(AuthConfig::parse("rw:token with-space").is_err());
    }

    #[test]
    fn erasure_is_an_admin_capability_not_a_method() {
        let auth = AuthConfig::parse("rw:writer-token,admin:root-token").unwrap();
        assert_eq!(
            auth.scope_for(Some("Bearer root-token")),
            Ok(Scope::Admin),
            "admin parses and authenticates like any scope"
        );
        assert!(Scope::Admin.permits("POST") && Scope::Admin.permits("GET"));
        assert!(Scope::Admin.permits_erasure());
        // The write scope every collector holds must NOT erase: a credential
        // minted to produce telemetry cannot be the credential that destroys
        // it.
        assert!(!Scope::ReadWrite.permits_erasure());
        assert!(!Scope::ReadOnly.permits_erasure());
    }

    #[test]
    fn tenant_bindings_parse_and_reach_the_principal() {
        let auth = AuthConfig::parse("rw@acme:tok-a,ro:tok-open,admin@big.co:tok-b").unwrap();
        assert_eq!(
            auth.principal_for(Some("Bearer tok-a")),
            Ok(Principal {
                scope: Scope::ReadWrite,
                tenant: Some("acme".to_owned()),
                credential: None,
            })
        );
        assert_eq!(
            auth.principal_for(Some("Bearer tok-open")),
            Ok(Principal {
                scope: Scope::ReadOnly,
                tenant: None,
                credential: None,
            })
        );
        assert_eq!(
            auth.principal_for(Some("Bearer tok-b")),
            Ok(Principal {
                scope: Scope::Admin,
                tenant: Some("big.co".to_owned()),
                credential: None,
            })
        );
        // A token may contain '@' or ':' — the first colon decides, and the
        // binding is examined only left of it.
        let auth = AuthConfig::parse("rw:t@ok:en").unwrap();
        assert_eq!(
            auth.principal_for(Some("Bearer t@ok:en")).map(|p| p.tenant),
            Ok(None)
        );
        // Invalid bindings are configuration errors, loudly.
        assert!(AuthConfig::parse("rw@:tok").is_err());
        assert!(AuthConfig::parse("rw@Acme:tok").is_err());
        assert!(AuthConfig::parse("bogus@acme:tok").is_err());
    }

    #[test]
    fn constant_time_comparison_handles_different_lengths() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"different"));
        assert!(!constant_time_eq(b"prefix", b"prefix-extra"));
    }
}
