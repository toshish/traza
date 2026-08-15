//! Bearer-token authentication for the HTTP server.
//!
//! Authentication configuration is intentionally kept out of logs and error
//! messages. `AuthConfig` also implements a redacted `Debug` representation so
//! an accidental structured log cannot disclose configured credentials.

use std::env;
use std::fmt;

/// Environment variable holding `token:scope` credentials (see README).
pub const AUTH_ENV: &str = "TRAZA_TOKENS";
/// JSON body returned with 401 responses.
pub const UNAUTHORIZED_BODY: &str = "{\"error\":\"unauthorized\"}";
/// JSON body returned with 403 responses.
pub const FORBIDDEN_BODY: &str = "{\"error\":\"forbidden\"}";
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
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ro" => Some(Self::ReadOnly),
            "rw" => Some(Self::ReadWrite),
            "admin" => Some(Self::Admin),
            _ => None,
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

#[derive(Clone)]
struct Credential {
    token: Vec<u8>,
    scope: Scope,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Why a request was refused: unknown token (401) or insufficient scope (403).
pub enum AuthFailure {
    /// No, malformed, or unknown bearer token (HTTP 401).
    Unauthorized,
    /// Valid token without the scope for this method (HTTP 403).
    Forbidden,
}

impl AuthFailure {
    /// The HTTP status for this failure (401 or 403).
    pub fn status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
        }
    }

    /// The JSON error body for this failure.
    pub fn body(self) -> &'static str {
        match self {
            Self::Unauthorized => UNAUTHORIZED_BODY,
            Self::Forbidden => FORBIDDEN_BODY,
        }
    }

    /// The WWW-Authenticate challenge, present on 401.
    pub fn www_authenticate(self) -> Option<&'static str> {
        match self {
            Self::Unauthorized => Some(WWW_AUTHENTICATE),
            Self::Forbidden => None,
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

    /// Parses a TRAZA_TOKENS value (`scope:token`, comma-separated).
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
        let scope = self.scope_for(authorization)?;
        if scope.permits(method) {
            Ok(())
        } else {
            Err(AuthFailure::Forbidden)
        }
    }

    /// Authenticates a bearer token and returns what it may do, without
    /// applying the HTTP-method rule.
    ///
    /// The method rule is right for the REST surface, where the method *is*
    /// the operation. It is wrong for a protocol that tunnels every operation
    /// through one `POST`: applied there it would lock `ro` tokens out of a
    /// read-only surface entirely, and hand every caller that got in the write
    /// scope. [`crate::mcp`] authorizes per tool instead, and this is the
    /// authentication half it builds on.
    pub fn scope_for(&self, authorization: Option<&str>) -> Result<Scope, AuthFailure> {
        let token = authorization
            .and_then(parse_bearer)
            .ok_or(AuthFailure::Unauthorized)?;

        // Check every configured credential even after a match. Besides avoiding
        // an early token-dependent return, this keeps lookup timing independent
        // of credential ordering.
        let mut matched_scope = None;
        for credential in &self.credentials {
            let matched = constant_time_eq(&credential.token, token.as_bytes());
            if matched {
                matched_scope = Some(credential.scope);
            }
        }
        matched_scope.ok_or(AuthFailure::Unauthorized)
    }
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
            Err(AuthFailure::Forbidden)
        );
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
    fn constant_time_comparison_handles_different_lengths() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"different"));
        assert!(!constant_time_eq(b"prefix", b"prefix-extra"));
    }
}
