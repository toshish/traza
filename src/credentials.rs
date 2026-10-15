//! Runtime credentials: who may call, minted and revoked while the server
//! runs, and kept on disk only as digests.
//!
//! `TRAZA_TOKENS` made rotation a restart, which a hosted control plane
//! cannot live with: issuing a customer a token must not bounce every other
//! customer's connection. This module is the credential set the server
//! authenticates against — the environment's entries (`source: "env"`) plus
//! every credential an unbound admin minted over HTTP (`source: "api"`).
//!
//! **Storage.** `credentials.jsonl` in the data directory: append-only
//! `create` and `revoke` records, fsynced per write, a torn final append
//! healed at open exactly as the annotation log heals one. The token itself
//! is never written — only its SHA-256 — so a copy of the data directory is
//! not a copy of anyone's credentials.
//!
//! **Outside generations and pins.** The file is not in the manifest's
//! whitelist ([`crate::generation`]), so a backup never carries it and a
//! restore never replaces it: restoring last month's backup must not
//! resurrect a token revoked last week, and a backup handed to someone for
//! analysis must not hand them working digests either.
//!
//! **Lookup is by digest.** The presented bearer is hashed and the digest
//! looked up in a map. The probe's timing depends on the DIGEST, not on the
//! token, and learning bytes of a SHA-256 output does not help anyone
//! construct a preimage — which is what lets this replace the per-credential
//! constant-time scan [`crate::auth::AuthConfig`] does without reopening a
//! timing channel. Environment tokens are hashed at install and looked up the
//! same way, so there is one lookup path, not two.
//!
//! **Posture.** With no environment credentials and no API credential ever
//! created, [`CredentialStore::enforced`] is false and the server stays open
//! exactly as before (loopback by default). The first credential, from either
//! source, turns enforcement on — and it stays on: revoking every credential
//! locks the store rather than reopening it, because "the last revoke made the
//! server public" is not a failure mode anyone would choose.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{AuthConfig, AuthFailure, CredentialRef, Principal, Scope};
use crate::payload::sha256_hex;
use crate::{Error, Result};

/// The credential log's file name inside the data directory.
pub const LOG_NAME: &str = "credentials.jsonl";

/// Shortest token a caller may supply instead of having one minted. A control
/// plane reconciling credentials it already issued needs to register ITS
/// token; the floor keeps that from being a way to install `password1`.
pub const MIN_SUPPLIED_TOKEN_BYTES: usize = 24;

/// Longest supplied token. Tokens ride in a header capped at 64 KiB; this is
/// far below that and far above any real generator's output.
const MAX_SUPPLIED_TOKEN_BYTES: usize = 1024;

/// Longest label, in bytes. A label is a note for a human, not a document.
const MAX_LABEL_BYTES: usize = 256;

/// Characters of a token shown as its prefix, at most. See [`token_prefix`].
const MAX_PREFIX_CHARS: usize = 12;

/// Where a credential came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Minted or registered over `POST /v1/admin/credentials`, persisted.
    Api,
    /// Configured in `TRAZA_TOKENS`, held in memory for this process only.
    Env,
}

/// What `POST /v1/admin/credentials` asks for.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredential {
    /// `ro`, `rw` or `admin`.
    pub scope: String,
    /// The tenant to bind, or absent for an unbound credential.
    #[serde(default)]
    pub tenant: Option<String>,
    /// A note for whoever reads the credential list later.
    #[serde(default)]
    pub label: Option<String>,
    /// Lifetime from now; absent never expires.
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
    /// A token to register instead of minting one. At least
    /// [`MIN_SUPPLIED_TOKEN_BYTES`], no whitespace, no commas.
    #[serde(default)]
    pub token: Option<String>,
}

/// A credential just created. The token is in here and NOWHERE else, ever:
/// the store keeps only its digest, so this is the one chance to read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Created {
    /// The credential id.
    pub id: String,
    /// The bearer token.
    pub token: String,
    /// The first few characters of the token, for recognising it later.
    pub prefix: String,
    /// The granted scope.
    pub scope: Scope,
    /// The tenant binding, if any.
    pub tenant: Option<String>,
    /// The label; empty when none was given.
    pub label: String,
    /// When it was created.
    pub created_at_ns: u64,
    /// When it stops authenticating, if ever.
    pub expires_at_ns: Option<u64>,
}

impl Created {
    /// The `201` body: the documented wire shape, token included.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "token": self.token,
            "prefix": self.prefix,
            "scope": self.scope.as_str(),
            "tenant": self.tenant,
            "label": self.label,
            "created_at_ns": self.created_at_ns,
            "expires_at_ns": self.expires_at_ns,
        })
    }
}

/// One credential as the list shows it: everything about it except anything
/// derived from the token beyond its short prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CredentialView {
    /// The credential id.
    pub id: String,
    /// The first few characters of the token.
    pub prefix: String,
    /// `ro`, `rw` or `admin`.
    pub scope: &'static str,
    /// The tenant binding, or `null` when unbound.
    pub tenant: Option<String>,
    /// The label; empty when none was given.
    pub label: String,
    /// When it was created (for an environment credential, when this
    /// process loaded it).
    pub created_at_ns: u64,
    /// When it stops authenticating, if ever.
    pub expires_at_ns: Option<u64>,
    /// When it was revoked, if it was.
    pub revoked_at_ns: Option<u64>,
    /// When it last authenticated a request in THIS process, if it has.
    /// In memory only: a restart forgets it, and recording it durably would
    /// put an fsync on every authenticated request.
    pub last_used_at_ns: Option<u64>,
    /// `api` or `env`.
    pub source: Source,
}

/// The outcome of a revoke.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Revoked {
    /// Revoked now, or already revoked at this instant (idempotent).
    At(u64),
    /// No credential has that id.
    Unknown,
}

/// One line of `credentials.jsonl`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Record {
    Create {
        id: String,
        /// SHA-256 of the token, lowercase hex. The token is never stored.
        digest: String,
        prefix: String,
        scope: String,
        tenant: Option<String>,
        label: String,
        created_at_ns: u64,
        expires_at_ns: Option<u64>,
    },
    Revoke {
        id: String,
        revoked_at_ns: u64,
    },
}

#[derive(Debug)]
struct Entry {
    id: String,
    digest: String,
    prefix: String,
    scope: Scope,
    tenant: Option<String>,
    label: String,
    created_at_ns: u64,
    expires_at_ns: Option<u64>,
    revoked_at_ns: Option<u64>,
    /// Zero means never. Atomic so authentication, which holds only the read
    /// lock, can record use without serializing requests on a write lock.
    last_used_at_ns: AtomicU64,
    source: Source,
}

impl Entry {
    fn view(&self) -> CredentialView {
        let last_used = self.last_used_at_ns.load(Ordering::Relaxed);
        CredentialView {
            id: self.id.clone(),
            prefix: self.prefix.clone(),
            scope: self.scope.as_str(),
            tenant: self.tenant.clone(),
            label: self.label.clone(),
            created_at_ns: self.created_at_ns,
            expires_at_ns: self.expires_at_ns,
            revoked_at_ns: self.revoked_at_ns,
            last_used_at_ns: (last_used > 0).then_some(last_used),
            source: self.source,
        }
    }

    fn live_at(&self, now_ns: u64) -> bool {
        self.revoked_at_ns.is_none() && self.expires_at_ns.map_or(true, |at| now_ns < at)
    }
}

#[derive(Debug, Default)]
struct Inner {
    entries: Vec<Entry>,
    by_digest: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
    /// Whether the log holds any `create` record — revoked or expired ones
    /// included, which is the point: see the module docs on posture.
    api_ever: bool,
}

impl Inner {
    fn push(&mut self, entry: Entry) {
        let position = self.entries.len();
        self.by_digest.insert(entry.digest.clone(), position);
        self.by_id.insert(entry.id.clone(), position);
        if entry.source == Source::Api {
            self.api_ever = true;
        }
        self.entries.push(entry);
    }

    fn reindex(&mut self) {
        self.by_digest.clear();
        self.by_id.clear();
        for (position, entry) in self.entries.iter().enumerate() {
            self.by_digest.insert(entry.digest.clone(), position);
            self.by_id.insert(entry.id.clone(), position);
        }
    }

    fn has_env(&self) -> bool {
        self.entries.iter().any(|entry| entry.source == Source::Env)
    }
}

/// The credential set a server authenticates against.
#[derive(Debug)]
pub struct CredentialStore {
    path: PathBuf,
    inner: RwLock<Inner>,
}

impl CredentialStore {
    /// Opens the credential log in `directory`, healing a torn final append.
    ///
    /// A malformed record anywhere but the unterminated tail is corruption
    /// and refuses the open: silently dropping a `revoke` would resurrect the
    /// token it revoked.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut inner = Inner::default();
        for record in replay(&path, true)? {
            apply(&mut inner, record)?;
        }
        Ok(Self {
            path,
            inner: RwLock::new(inner),
        })
    }

    /// Whether `directory` holds a credential log with at least one complete
    /// `create` record, without healing or otherwise writing anything.
    ///
    /// For the server's startup check, which runs before the store is opened
    /// (and therefore before the directory lock is held): a non-empty log is
    /// configured authentication, so a non-loopback bind may proceed without
    /// `TRAZA_TOKENS`.
    pub fn has_api_credentials(directory: &Path) -> Result<bool> {
        Ok(replay(&directory.join(LOG_NAME), false)?
            .iter()
            .any(|record| matches!(record, Record::Create { .. })))
    }

    /// Installs `TRAZA_TOKENS`'s entries, replacing any installed before.
    ///
    /// An environment token that is ALSO a runtime credential refuses: two
    /// records with one digest would make which scope a request gets depend
    /// on lookup order, and if the runtime one was revoked, the environment
    /// would be silently resurrecting it.
    pub fn install_env(&self, config: &AuthConfig, now_ns: u64) -> Result<()> {
        let mut inner = self.write()?;
        inner.entries.retain(|entry| entry.source == Source::Api);
        inner.reindex();
        for (token, scope, tenant) in config.entries() {
            let digest = sha256_hex(token);
            if let Some(&position) = inner.by_digest.get(&digest) {
                return Err(Error::Invalid(format!(
                    "TRAZA_TOKENS contains a token already registered as runtime credential {}",
                    inner.entries[position].id
                )));
            }
            let id = env_credential_id(&digest);
            inner.push(Entry {
                id,
                prefix: token_prefix(&String::from_utf8_lossy(token)),
                digest,
                scope,
                tenant: tenant.map(str::to_owned),
                label: String::new(),
                created_at_ns: now_ns,
                expires_at_ns: None,
                revoked_at_ns: None,
                last_used_at_ns: AtomicU64::new(0),
                source: Source::Env,
            });
        }
        Ok(())
    }

    /// Whether requests must authenticate: true once any credential exists,
    /// from either source, and forever after for API credentials.
    pub fn enforced(&self) -> bool {
        self.inner
            .read()
            .map(|inner| inner.api_ever || inner.has_env())
            // A poisoned lock fails CLOSED: enforcement on, and every lookup
            // below then refuses as unauthorized.
            .unwrap_or(true)
    }

    /// Authenticates an `Authorization` header value at `now_ns`.
    ///
    /// Unknown, expired and revoked tokens are all `Unauthorized`, with the
    /// same body: which of the three a token is would be an oracle for
    /// somebody holding a leaked one.
    pub fn authenticate(
        &self,
        authorization: Option<&str>,
        now_ns: u64,
    ) -> std::result::Result<Principal, AuthFailure> {
        let token = authorization
            .and_then(crate::auth::bearer_token)
            .ok_or(AuthFailure::Unauthorized)?;
        let digest = sha256_hex(token.as_bytes());
        let inner = self.inner.read().map_err(|_| AuthFailure::Unauthorized)?;
        let entry = inner
            .by_digest
            .get(&digest)
            .map(|&position| &inner.entries[position])
            .filter(|entry| entry.live_at(now_ns))
            .ok_or(AuthFailure::Unauthorized)?;
        entry
            .last_used_at_ns
            .store(now_ns.max(1), Ordering::Relaxed);
        Ok(Principal {
            scope: entry.scope,
            tenant: entry.tenant.clone(),
            credential: Some(CredentialRef {
                id: entry.id.clone(),
                label: entry.label.clone(),
            }),
        })
    }

    /// Creates a credential, minting its token unless one was supplied.
    ///
    /// The record is fsynced before this returns, and the in-memory set is
    /// updated under the same write lock as the append, so the credential
    /// authenticates from the moment the caller can read its token — and a
    /// duplicate digest is refused atomically. Invalid requests are
    /// [`Error::Invalid`]; a token already registered (live, expired,
    /// revoked, or in `TRAZA_TOKENS`) is [`Error::Conflict`] — a revoked token
    /// can never come back.
    pub fn create(&self, request: NewCredential, now_ns: u64) -> Result<Created> {
        let scope = Scope::parse(&request.scope).ok_or_else(|| {
            Error::Invalid(format!(
                "scope must be ro, rw or admin (got {:?})",
                request.scope
            ))
        })?;
        if let Some(tenant) = &request.tenant {
            if !crate::valid_tenant(tenant) {
                return Err(Error::Invalid(
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes; omit it \
                     for an unbound credential"
                        .to_owned(),
                ));
            }
        }
        let label = request.label.unwrap_or_default();
        if label.len() > MAX_LABEL_BYTES {
            return Err(Error::Invalid(format!(
                "label is {} bytes; at most {MAX_LABEL_BYTES}",
                label.len()
            )));
        }
        let expires_at_ns = match request.expires_in_seconds {
            None => None,
            Some(0) => {
                return Err(Error::Invalid(
                    "expires_in_seconds must be positive; omit it for a credential that \
                     does not expire"
                        .to_owned(),
                ))
            }
            Some(seconds) => Some(now_ns.saturating_add(seconds.saturating_mul(1_000_000_000))),
        };
        let token = match request.token {
            Some(token) => {
                validate_supplied_token(&token)?;
                token
            }
            None => format!("trz_{}", random_hex(32)?),
        };
        let digest = sha256_hex(token.as_bytes());

        let mut inner = self.write()?;
        if inner.by_digest.contains_key(&digest) {
            return Err(Error::Conflict(
                "a credential with this token already exists (tokens are never reused, \
                 revoked ones included)"
                    .to_owned(),
            ));
        }
        let id = loop {
            let candidate = format!("cr_{}", random_hex(8)?);
            if !inner.by_id.contains_key(&candidate) {
                break candidate;
            }
        };
        let prefix = token_prefix(&token);
        let record = Record::Create {
            id: id.clone(),
            digest: digest.clone(),
            prefix: prefix.clone(),
            scope: scope.as_str().to_owned(),
            tenant: request.tenant.clone(),
            label: label.clone(),
            created_at_ns: now_ns,
            expires_at_ns,
        };
        self.append(&record)?;
        inner.push(Entry {
            id: id.clone(),
            digest,
            prefix: prefix.clone(),
            scope,
            tenant: request.tenant.clone(),
            label: label.clone(),
            created_at_ns: now_ns,
            expires_at_ns,
            revoked_at_ns: None,
            last_used_at_ns: AtomicU64::new(0),
            source: Source::Api,
        });
        Ok(Created {
            id,
            token,
            prefix,
            scope,
            tenant: request.tenant,
            label,
            created_at_ns: now_ns,
            expires_at_ns,
        })
    }

    /// Every credential, environment ones first, then API ones in creation
    /// order.
    pub fn list(&self) -> Result<Vec<CredentialView>> {
        let inner = self.read()?;
        let mut views: Vec<CredentialView> = inner.entries.iter().map(Entry::view).collect();
        views.sort_by_key(|view| (view.source != Source::Env, view.created_at_ns));
        Ok(views)
    }

    /// Revokes `id` at `now_ns`. Idempotent: revoking a revoked credential
    /// answers with its original revocation time and writes nothing. An
    /// environment credential is [`Error::Conflict`] — it is not this store's
    /// to revoke, and pretending would last only until the next restart.
    pub fn revoke(&self, id: &str, now_ns: u64) -> Result<Revoked> {
        let mut inner = self.write()?;
        let Some(&position) = inner.by_id.get(id) else {
            return Ok(Revoked::Unknown);
        };
        let entry = &inner.entries[position];
        if entry.source == Source::Env {
            return Err(Error::Conflict(
                "env credentials are revoked by restarting without them".to_owned(),
            ));
        }
        if let Some(at) = entry.revoked_at_ns {
            return Ok(Revoked::At(at));
        }
        self.append(&Record::Revoke {
            id: id.to_owned(),
            revoked_at_ns: now_ns,
        })?;
        inner.entries[position].revoked_at_ns = Some(now_ns);
        Ok(Revoked::At(now_ns))
    }

    fn append(&self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let created = !self.path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        if created {
            if let Some(directory) = self.path.parent() {
                crate::sync_directory(directory)?;
            }
        }
        Ok(())
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Inner>> {
        self.inner
            .read()
            .map_err(|_| Error::LockPoisoned("credentials"))
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Inner>> {
        self.inner
            .write()
            .map_err(|_| Error::LockPoisoned("credentials"))
    }
}

/// Reads every complete record. With `heal`, an unterminated malformed tail is
/// truncated away (and a complete record missing only its newline gets one),
/// so the next append cannot concatenate onto it; without, the tail is merely
/// skipped and nothing is written.
fn replay(path: &Path, heal: bool) -> Result<Vec<Record>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let terminated = contents.ends_with(b"\n");
    let mut lines = contents.split_inclusive(|byte| *byte == b'\n').peekable();
    let mut records = Vec::new();
    let mut valid_len = 0_u64;
    let mut torn = false;
    while let Some(line) = lines.next() {
        if line.iter().all(u8::is_ascii_whitespace) {
            valid_len = valid_len.saturating_add(line.len() as u64);
            continue;
        }
        match serde_json::from_slice::<Record>(line) {
            Ok(record) => {
                valid_len = valid_len.saturating_add(line.len() as u64);
                records.push(record);
            }
            // Only the final, unterminated append can be torn by a crash.
            Err(_) if lines.peek().is_none() && !terminated => {
                torn = true;
                break;
            }
            Err(error) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is corrupt before its final record ({error}); refusing to guess \
                         which credentials it held",
                        path.display()
                    ),
                )))
            }
        }
    }
    if heal && torn {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(valid_len)?;
        file.sync_all()?;
    } else if heal && !contents.is_empty() && !terminated {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    Ok(records)
}

fn apply(inner: &mut Inner, record: Record) -> Result<()> {
    let corrupt = |why: String| {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{LOG_NAME}: {why}"),
        ))
    };
    match record {
        Record::Create {
            id,
            digest,
            prefix,
            scope,
            tenant,
            label,
            created_at_ns,
            expires_at_ns,
        } => {
            let scope = Scope::parse(&scope)
                .ok_or_else(|| corrupt(format!("credential {id} has unknown scope {scope:?}")))?;
            if inner.by_id.contains_key(&id) || inner.by_digest.contains_key(&digest) {
                return Err(corrupt(format!("credential {id} is recorded twice")));
            }
            inner.push(Entry {
                id,
                digest,
                prefix,
                scope,
                tenant,
                label,
                created_at_ns,
                expires_at_ns,
                revoked_at_ns: None,
                last_used_at_ns: AtomicU64::new(0),
                source: Source::Api,
            });
        }
        Record::Revoke { id, revoked_at_ns } => {
            let position = *inner
                .by_id
                .get(&id)
                .ok_or_else(|| corrupt(format!("revoke names unknown credential {id}")))?;
            let entry = &mut inner.entries[position];
            entry.revoked_at_ns.get_or_insert(revoked_at_ns);
        }
    }
    Ok(())
}

fn validate_supplied_token(token: &str) -> Result<()> {
    if token.len() < MIN_SUPPLIED_TOKEN_BYTES {
        return Err(Error::Invalid(format!(
            "a supplied token must be at least {MIN_SUPPLIED_TOKEN_BYTES} bytes"
        )));
    }
    if token.len() > MAX_SUPPLIED_TOKEN_BYTES {
        return Err(Error::Invalid(format!(
            "a supplied token must be at most {MAX_SUPPLIED_TOKEN_BYTES} bytes"
        )));
    }
    // The same characters a bearer header can carry: anything the header
    // parser would refuse is a token that could never authenticate.
    if token
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || ch == ',')
    {
        return Err(Error::Invalid(
            "a supplied token must not contain whitespace, control characters or commas".to_owned(),
        ));
    }
    Ok(())
}

/// The part of a token safe to show: at most a quarter of it, never more than
/// [`MAX_PREFIX_CHARS`] characters. A minted `trz_<64 hex>` shows `trz_` and
/// eight hex digits — 32 of its 256 random bits — which is enough to tell
/// tokens apart and nowhere near enough to guess one. The quarter rule keeps
/// a short supplied or environment token from being mostly disclosed.
pub fn token_prefix(token: &str) -> String {
    let chars = token.chars().count();
    token
        .chars()
        .take((chars / 4).min(MAX_PREFIX_CHARS))
        .collect()
}

/// The id of an environment credential: stable across restarts (it is never
/// persisted, so it cannot be random), derived from the digest under its own
/// domain so the id is not itself a prefix of the digest.
fn env_credential_id(digest: &str) -> String {
    let derived = sha256_hex(format!("traza-credential-id/1\n{digest}").as_bytes());
    format!("cr_{}", &derived[..16])
}

/// `bytes` bytes from the operating system's CSPRNG, as lowercase hex.
///
/// Read from `/dev/urandom`, which every supported platform has and which
/// never blocks once the system is booted. Where it does not exist the
/// error says so and names the alternative — supplying a token — rather than
/// falling back to anything weaker: a guessable credential is worse than no
/// credential, because it looks like one.
fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut buffer))
        .map_err(|error| {
            Error::Io(io::Error::new(
                error.kind(),
                format!(
                    "cannot read the OS randomness source /dev/urandom ({error}); supply a \
                     \"token\" of your own instead"
                ),
            ))
        })?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Counter;

    static NEXT: Counter = Counter::new(0);

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "traza-credentials-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("dir");
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(scope: &str) -> NewCredential {
        NewCredential {
            scope: scope.to_owned(),
            ..NewCredential::default()
        }
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    #[test]
    fn a_minted_token_authenticates_and_only_its_digest_reaches_disk() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        assert!(!store.enforced(), "an empty set is open mode");
        let created = store
            .create(
                NewCredential {
                    scope: "rw".into(),
                    tenant: Some("acme".into()),
                    label: Some("collector".into()),
                    ..NewCredential::default()
                },
                1_000,
            )
            .expect("creates");
        assert!(created.id.starts_with("cr_") && created.id.len() == 19);
        assert!(created.token.starts_with("trz_") && created.token.len() == 68);
        assert!(created.token[4..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(created.prefix, &created.token[..12]);
        assert!(
            store.enforced(),
            "the first credential turns enforcement on"
        );

        let principal = store
            .authenticate(Some(&bearer(&created.token)), 2_000)
            .expect("authenticates");
        assert_eq!(principal.scope, Scope::ReadWrite);
        assert_eq!(principal.tenant.as_deref(), Some("acme"));
        assert_eq!(
            principal.credential,
            Some(CredentialRef {
                id: created.id.clone(),
                label: "collector".into()
            })
        );

        let on_disk = fs::read_to_string(dir.0.join(LOG_NAME)).expect("log");
        assert!(!on_disk.contains(&created.token), "{on_disk}");
        assert!(on_disk.contains(&sha256_hex(created.token.as_bytes())));

        let listed = store.list().expect("lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last_used_at_ns, Some(2_000));
        let wire = serde_json::to_string(&listed).expect("serializes");
        assert!(!wire.contains(&created.token[12..]), "{wire}");
        assert!(!wire.contains("digest"), "{wire}");
    }

    #[test]
    fn revocation_and_expiry_survive_a_reopen_and_refuse_alike() {
        let dir = Dir::new();
        let (revoked, expiring, kept) = {
            let store = CredentialStore::open(&dir.0).expect("opens");
            let revoked = store.create(request("admin"), 10).expect("creates");
            let expiring = store
                .create(
                    NewCredential {
                        expires_in_seconds: Some(5),
                        ..request("ro")
                    },
                    10,
                )
                .expect("creates");
            let kept = store.create(request("ro"), 10).expect("creates");
            assert_eq!(
                store.revoke(&revoked.id, 20).expect("revokes"),
                Revoked::At(20)
            );
            assert_eq!(
                store.revoke(&revoked.id, 30).expect("idempotent"),
                Revoked::At(20),
                "a second revoke keeps the first instant"
            );
            assert_eq!(store.revoke("cr_nope", 30).expect("ok"), Revoked::Unknown);
            (revoked, expiring, kept)
        };
        let store = CredentialStore::open(&dir.0).expect("reopens");
        let now = 10 + 5_000_000_000;
        assert_eq!(
            store.authenticate(Some(&bearer(&revoked.token)), 25),
            Err(AuthFailure::Unauthorized),
            "a revoked token stays revoked across a restart"
        );
        assert!(store
            .authenticate(Some(&bearer(&expiring.token)), now - 1)
            .is_ok());
        assert_eq!(
            store.authenticate(Some(&bearer(&expiring.token)), now),
            Err(AuthFailure::Unauthorized),
            "expiry is exclusive of its instant"
        );
        assert!(store.authenticate(Some(&bearer(&kept.token)), now).is_ok());
        let listed = store.list().expect("lists");
        assert_eq!(listed[0].revoked_at_ns, Some(20));
        assert_eq!(listed[0].last_used_at_ns, None, "last use is memory only");
    }

    #[test]
    fn revoking_everything_locks_the_store_rather_than_opening_it() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let only = store.create(request("admin"), 1).expect("creates");
        store.revoke(&only.id, 2).expect("revokes");
        assert!(store.enforced());
        drop(store);
        let store = CredentialStore::open(&dir.0).expect("reopens");
        assert!(
            store.enforced(),
            "a log of revoked credentials still enforces"
        );
        assert!(CredentialStore::has_api_credentials(&dir.0).expect("peeks"));
    }

    #[test]
    fn supplied_tokens_are_validated_and_never_reused() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let supplied = "control-plane-issued-0123456789";
        let created = store
            .create(
                NewCredential {
                    token: Some(supplied.into()),
                    ..request("rw")
                },
                1,
            )
            .expect("accepts a long enough token");
        assert_eq!(created.token, supplied);
        assert_eq!(created.prefix, "control");
        let duplicate = store.create(
            NewCredential {
                token: Some(supplied.into()),
                ..request("ro")
            },
            2,
        );
        assert!(
            matches!(duplicate, Err(Error::Conflict(_))),
            "{duplicate:?}"
        );
        store.revoke(&created.id, 3).expect("revokes");
        let again = store.create(
            NewCredential {
                token: Some(supplied.into()),
                ..request("ro")
            },
            4,
        );
        assert!(
            matches!(again, Err(Error::Conflict(_))),
            "a revoked token cannot be registered again: {again:?}"
        );

        for bad in [
            "short-token",
            "has a space in it 0123456789",
            "comma,separated-0123456789",
        ] {
            let refused = store.create(
                NewCredential {
                    token: Some(bad.into()),
                    ..request("rw")
                },
                5,
            );
            assert!(
                matches!(refused, Err(Error::Invalid(_))),
                "{bad}: {refused:?}"
            );
        }
        for (bad, field) in [
            (request("root"), "scope"),
            (
                NewCredential {
                    tenant: Some("Acme".into()),
                    ..request("rw")
                },
                "tenant",
            ),
            (
                NewCredential {
                    expires_in_seconds: Some(0),
                    ..request("rw")
                },
                "expires_in_seconds",
            ),
            (
                NewCredential {
                    label: Some("x".repeat(MAX_LABEL_BYTES + 1)),
                    ..request("rw")
                },
                "label",
            ),
        ] {
            match store.create(bad, 6) {
                Err(Error::Invalid(message)) => assert!(message.contains(field), "{message}"),
                other => panic!("{field}: {other:?}"),
            }
        }
        assert_eq!(
            store.list().expect("lists").len(),
            1,
            "refusals write nothing"
        );
    }

    #[test]
    fn environment_credentials_coexist_with_stable_ids_and_cannot_be_revoked() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let config = AuthConfig::parse("ro@acme:env-reader-token,admin:env-root").expect("parses");
        store.install_env(&config, 7).expect("installs");
        assert!(store.enforced());
        let reader = store
            .authenticate(Some("Bearer env-reader-token"), 8)
            .expect("authenticates");
        assert_eq!(reader.scope, Scope::ReadOnly);
        assert_eq!(reader.tenant.as_deref(), Some("acme"));
        let id = reader.credential.expect("has an id").id;
        assert_eq!(id, env_credential_id(&sha256_hex(b"env-reader-token")));

        let api = store.create(request("rw"), 9).expect("creates");
        let listed = store.list().expect("lists");
        assert_eq!(
            listed.iter().map(|view| view.source).collect::<Vec<_>>(),
            vec![Source::Env, Source::Env, Source::Api]
        );
        assert_eq!(listed[0].prefix, "env-", "a quarter of a 16-char token");
        assert!(matches!(store.revoke(&id, 10), Err(Error::Conflict(_))));
        assert!(
            store
                .create(
                    NewCredential {
                        token: Some("env-reader-token-padded-out".into()),
                        ..request("rw")
                    },
                    11
                )
                .is_ok(),
            "a token that merely starts like an environment one is distinct"
        );

        // An environment token that collides with a runtime one is refused.
        let colliding = AuthConfig::parse(&format!("rw:{}", api.token)).expect("parses");
        assert!(matches!(
            store.install_env(&colliding, 12),
            Err(Error::Invalid(_))
        ));
        // ... and a runtime create cannot claim an environment token.
        let long_env = "env-operator-token-0123456789";
        let config = AuthConfig::parse(&format!("admin:{long_env}")).expect("parses");
        store.install_env(&config, 13).expect("reinstalls");
        assert!(
            store.authenticate(Some("Bearer env-root"), 13).is_err(),
            "reinstalling replaces the previous environment set"
        );
        let claimed = store.create(
            NewCredential {
                token: Some(long_env.into()),
                ..request("rw")
            },
            14,
        );
        assert!(matches!(claimed, Err(Error::Conflict(_))), "{claimed:?}");
    }

    #[test]
    fn a_torn_final_append_is_healed_and_mid_file_damage_refuses() {
        let dir = Dir::new();
        let token = {
            let store = CredentialStore::open(&dir.0).expect("opens");
            store.create(request("rw"), 1).expect("creates").token
        };
        let path = dir.0.join(LOG_NAME);
        let mut file = OpenOptions::new().append(true).open(&path).expect("log");
        file.write_all(br#"{"op":"revoke","id":"cr_"#)
            .expect("tear");
        drop(file);
        assert!(CredentialStore::has_api_credentials(&dir.0).expect("peek ignores the tear"));
        let store = CredentialStore::open(&dir.0).expect("heals");
        assert!(store.authenticate(Some(&bearer(&token)), 2).is_ok());
        store
            .create(request("ro"), 3)
            .expect("appends after the heal");
        drop(store);
        let store = CredentialStore::open(&dir.0).expect("reopens cleanly");
        assert_eq!(store.list().expect("lists").len(), 2);
        drop(store);

        let mut contents = fs::read(&path).expect("log");
        contents.splice(0..0, b"not json\n".iter().copied());
        fs::write(&path, contents).expect("damage");
        let refused = CredentialStore::open(&dir.0);
        assert!(refused.is_err(), "mid-file damage must not be skipped");
    }

    #[test]
    fn malformed_headers_never_authenticate() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let created = store.create(request("rw"), 1).expect("creates");
        for header in [
            None,
            Some(created.token.clone()),
            Some(format!("bearer {}", created.token)),
            Some(format!("Bearer {} ", created.token)),
            Some(format!("Bearer {}x", created.token)),
        ] {
            assert_eq!(
                store.authenticate(header.as_deref(), 2),
                Err(AuthFailure::Unauthorized),
                "{header:?}"
            );
        }
    }

    #[test]
    fn prefixes_never_disclose_more_than_a_quarter() {
        assert_eq!(
            token_prefix(&format!("trz_{}", "a".repeat(64))),
            "trz_aaaaaaaa"
        );
        assert_eq!(token_prefix("abcdefgh"), "ab");
        assert_eq!(token_prefix("abc"), "");
    }
}
