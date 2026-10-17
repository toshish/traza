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
//! healed at open exactly as the annotation log heals one. An append that
//! fails while the server runs — a full disk, a failed fsync — is truncated
//! away at once, so the log never holds a record its caller was told failed
//! nor bytes the next record would land after; if even that fails, changes
//! refuse until a restart heals the log. The token itself
//! is never written — only its SHA-256 — so for a minted token (256 random
//! bits) a copy of the data directory is not a copy of anyone's credentials.
//! The digest is unsalted, which is sound only for high-entropy tokens: a
//! caller-supplied token is protected at rest by nothing but that digest, so
//! it must be random enough not to be guessed offline. The file is created
//! owner-only (`0600` on Unix) so the digests are not world-readable either.
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
//! server public" is not a failure mode anyone would choose. For the same
//! reason the first API credential must be an unbound admin: anything else
//! would switch enforcement on with no credential able to reach
//! `/v1/admin/*`, a lockout only a restart with `TRAZA_TOKENS` undoes. Open
//! mode ends for requests already in flight too: one admitted while the
//! store was open that reaches it after the first credential must
//! authenticate ([`Caller::Open`]), so of two racing bootstraps exactly one
//! creates anything.
//!
//! **Writers never stall readers on the disk.** Create and revoke serialize
//! on a mutex of their own and append and fsync under it alone; the set's
//! read-write lock is taken for writing only for the in-memory change, after
//! the record is durable. Authentication takes only the read lock, so it
//! never waits on an fsync.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{AuthConfig, AuthFailure, CredentialRef, Principal, Scope};
use crate::payload::sha256_hex;
use crate::{Error, Result};

/// The credential log's file name inside the data directory.
pub const LOG_NAME: &str = "credentials.jsonl";

/// Shortest token a caller may supply instead of having one minted. A control
/// plane reconciling credentials it already issued needs to register ITS
/// token; the floor keeps that from being a way to install `password1`. Length
/// is not entropy, though, and only an unsalted digest protects the token at
/// rest: a supplied token should carry at least 128 random bits.
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
///
/// `Debug` is written by hand to redact `token`, as
/// [`crate::auth::AuthConfig`]'s is: a supplied token is a live secret.
#[derive(Clone, Default, Deserialize)]
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
///
/// `Debug` is written by hand to redact `token` (`prefix` is shown — the
/// credential list shows it anyway): an error path or a log line that formats
/// this must not become a second place the token appears.
#[derive(Clone, Eq, PartialEq)]
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

impl fmt::Debug for NewCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewCredential")
            .field("scope", &self.scope)
            .field("tenant", &self.tenant)
            .field("label", &self.label)
            .field("expires_in_seconds", &self.expires_in_seconds)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl fmt::Debug for Created {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Created")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("prefix", &self.prefix)
            .field("scope", &self.scope)
            .field("tenant", &self.tenant)
            .field("label", &self.label)
            .field("created_at_ns", &self.created_at_ns)
            .field("expires_at_ns", &self.expires_at_ns)
            .finish()
    }
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

/// How the caller of a credential operation got past authentication.
///
/// The HTTP gate decides from a request's head whether the store is open,
/// and the head can arrive long before the body: a request admitted in open
/// mode may reach the store after another one minted the first credential.
/// So the store, not the gate, has the last word on whether open mode still
/// holds — see [`Caller::Open`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Caller {
    /// A credential the store authenticated — or the embedding process
    /// itself, which needs none.
    Authenticated,
    /// Nothing: the store enforced nothing when the request was admitted.
    /// Honoured only while it STILL enforces nothing, decided under the lock
    /// the operation itself takes — for a create, the writer lock, so of two
    /// bootstraps racing exactly one creates and the other is
    /// [`Error::Unauthenticated`] and must come back with a credential.
    Open,
}

impl Caller {
    /// The caller behind an authenticated principal, or open mode when there
    /// is none — the convention [`crate::platform::route`] receives.
    pub fn of(principal: Option<&Principal>) -> Self {
        match principal {
            Some(_) => Self::Authenticated,
            None => Self::Open,
        }
    }
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

    /// See [`CredentialStore::enforced`].
    fn enforced(&self) -> bool {
        self.api_ever || self.has_env()
    }

    /// Refuses an open-mode caller once the store enforces. Checked under the
    /// same lock as the step it guards, so "the store is still open" and the
    /// step are one decision; see [`Caller::Open`].
    fn admit(&self, caller: Caller) -> Result<()> {
        if caller == Caller::Open && self.enforced() {
            return Err(Error::Unauthenticated);
        }
        Ok(())
    }
}

/// The credential set a server authenticates against.
#[derive(Debug)]
pub struct CredentialStore {
    path: PathBuf,
    /// Serializes every change to the set — create, revoke, installing the
    /// environment's entries — so a writer can check the set under the read
    /// lock, append and fsync holding only this, and then apply its change
    /// under the write lock with no other writer having moved anything in
    /// between. Taken before `inner`, never after.
    writer: Mutex<Writer>,
    inner: RwLock<Inner>,
    #[cfg(test)]
    faults: Mutex<Faults>,
}

/// What only the writer may read or change: the log's standing.
#[derive(Debug, Default)]
struct Writer {
    /// Why the log can no longer be trusted to match memory: an append
    /// failed and undoing it failed too, so its tail holds bytes nobody can
    /// vouch for. Every later change refuses with this until a reopen heals
    /// the tail and re-reads the log; reads go on answering from memory,
    /// which holds exactly what was acknowledged.
    failed: Option<String>,
    /// Whether the log's directory entry is known durable. Set by the first
    /// append that succeeds, whichever one created the file — inferring it
    /// from "the file did not exist before this append" skipped the sync for
    /// good once a failed append had created the file and been rolled back.
    directory_synced: bool,
}

/// Failures a test injects into the next append, each consumed once.
#[cfg(test)]
#[derive(Debug, Default)]
struct Faults {
    /// The next write stops after this many bytes and fails, as a full disk
    /// does.
    torn_write: Option<usize>,
    /// The next fsync of the log fails, after its write landed whole.
    sync: bool,
    /// The next directory fsync fails.
    directory_sync: bool,
    /// The rollback of the next failed append fails too.
    rollback: bool,
    /// Directory fsyncs that succeeded, so a test can see one happen.
    directory_syncs: u32,
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
            writer: Mutex::new(Writer::default()),
            inner: RwLock::new(inner),
            #[cfg(test)]
            faults: Mutex::new(Faults::default()),
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
        let _writer = self.writer()?;
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
            .map(|inner| inner.enforced())
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
    /// The record is fsynced before the credential joins the in-memory set,
    /// and both happen before this returns, so the credential authenticates
    /// from the moment the caller can read its token and never before it is
    /// durable. Writers are serialized (the `writer` field), so a duplicate
    /// digest is refused atomically. Invalid requests are [`Error::Invalid`];
    /// a token already registered (live, expired, revoked, or in
    /// `TRAZA_TOKENS`) is [`Error::Conflict`] — a revoked token can never come
    /// back — and so is a first credential that is not an unbound admin (see
    /// the module docs on posture). An open-mode `caller` creates only while
    /// the store is still open, decided under the writer lock: one that lost
    /// the bootstrap is [`Error::Unauthenticated`].
    pub fn create(&self, request: NewCredential, caller: Caller, now_ns: u64) -> Result<Created> {
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

        let mut writer = self.writer()?;
        let id = {
            let inner = self.read()?;
            inner.admit(caller)?;
            let first = !inner.enforced();
            if first && (scope != Scope::Admin || request.tenant.is_some()) {
                return Err(Error::Conflict(
                    "the first credential must be an unbound admin (scope \"admin\", no \
                     tenant): any other would turn authentication on with no credential able \
                     to manage credentials"
                        .to_owned(),
                ));
            }
            if inner.by_digest.contains_key(&digest) {
                return Err(Error::Conflict(
                    "a credential with this token already exists (tokens are never reused, \
                     revoked ones included)"
                        .to_owned(),
                ));
            }
            loop {
                let candidate = format!("cr_{}", random_hex(8)?);
                if !inner.by_id.contains_key(&candidate) {
                    break candidate;
                }
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
        self.append(&mut writer, &record)?;
        self.write()?.push(Entry {
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
    /// order. An open-mode `caller` sees the (necessarily empty) list only
    /// while the store is still open: once a credential exists, listing it
    /// takes one.
    pub fn list(&self, caller: Caller) -> Result<Vec<CredentialView>> {
        let inner = self.read()?;
        inner.admit(caller)?;
        let mut views: Vec<CredentialView> = inner.entries.iter().map(Entry::view).collect();
        views.sort_by_key(|view| (view.source != Source::Env, view.created_at_ns));
        Ok(views)
    }

    /// Revokes `id` at `now_ns`. Idempotent: revoking a revoked credential
    /// answers with its original revocation time and writes nothing. An
    /// environment credential is [`Error::Conflict`] — it is not this store's
    /// to revoke, and pretending would last only until the next restart.
    ///
    /// The token stops authenticating before this returns, once the revoke
    /// record is durable — so from the caller's next request on. A request
    /// racing the revoke itself may still be admitted, exactly as it would
    /// have been had it arrived a moment earlier. An open-mode `caller` is
    /// refused once the store enforces, as for [`Self::create`].
    pub fn revoke(&self, id: &str, caller: Caller, now_ns: u64) -> Result<Revoked> {
        let mut writer = self.writer()?;
        let position = {
            let inner = self.read()?;
            inner.admit(caller)?;
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
            position
        };
        self.append(
            &mut writer,
            &Record::Revoke {
                id: id.to_owned(),
                revoked_at_ns: now_ns,
            },
        )?;
        // `position` is still this entry's: only a writer moves entries, and
        // this thread is the writer until it returns.
        self.write()?.entries[position].revoked_at_ns = Some(now_ns);
        Ok(Revoked::At(now_ns))
    }

    /// Appends `record` durably, or leaves the log as it found it.
    ///
    /// A failed append is undone — truncated back to the length it started
    /// at, and that fsynced — because what it leaves behind is worse than
    /// nothing. A write torn by a full disk is bytes the NEXT append would
    /// land after, turning a log of acknowledged records into one no restart
    /// can read; a whole record whose fsync failed is a credential its
    /// caller was told does not exist, which a retry with the same supplied
    /// token would record a second time, and two records of one digest
    /// refuse the open just the same. If the undo fails too, nobody can say
    /// what the tail holds, and the store latches ([`Writer::failed`]).
    fn append(&self, writer: &mut Writer, record: &Record) -> Result<()> {
        if let Some(why) = &writer.failed {
            return Err(Error::Io(io::Error::other(why.clone())));
        }
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        // Owner-only from the moment it exists. The file holds digests, not
        // tokens, but the digest of a weak supplied token can be guessed
        // offline, and nothing but the server needs to read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        // Where this record starts. Nothing else moves the end of the file
        // meanwhile: the directory lock keeps other processes out, and the
        // writer mutex, which the caller holds, other threads.
        let start = file.metadata()?.len();
        let appended = self
            .write_line(&mut file, line.as_bytes())
            .and_then(|()| self.sync_log(&file))
            .map_err(Error::from)
            .and_then(|()| {
                if writer.directory_synced {
                    Ok(())
                } else {
                    self.sync_log_directory()
                }
            });
        let Err(error) = appended else {
            writer.directory_synced = true;
            return Ok(());
        };
        if let Err(undo) = self.roll_back(&file, start) {
            writer.failed = Some(format!(
                "{}: an append failed ({error}) and truncating it away failed too ({undo}); \
                 credentials cannot change until a restart heals and re-reads the log",
                self.path.display()
            ));
        }
        Err(error)
    }

    fn roll_back(&self, file: &File, length: u64) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults().rollback) {
            return Err(io::Error::other("injected: the rollback failed"));
        }
        file.set_len(length)?;
        file.sync_all()
    }

    fn write_line(&self, file: &mut File, line: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(cut) = self.faults().torn_write.take() {
            file.write_all(&line[..cut.min(line.len())])?;
            return Err(io::Error::other("injected: the disk filled mid-write"));
        }
        file.write_all(line)
    }

    fn sync_log(&self, file: &File) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults().sync) {
            return Err(io::Error::other("injected: fsync failed"));
        }
        file.sync_all()
    }

    fn sync_log_directory(&self) -> Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults().directory_sync) {
            return Err(io::Error::other("injected: directory fsync failed").into());
        }
        if let Some(directory) = self.path.parent() {
            crate::sync_directory(directory)?;
        }
        #[cfg(test)]
        {
            self.faults().directory_syncs += 1;
        }
        Ok(())
    }

    #[cfg(test)]
    fn faults(&self) -> std::sync::MutexGuard<'_, Faults> {
        self.faults.lock().expect("faults")
    }

    fn writer(&self) -> Result<std::sync::MutexGuard<'_, Writer>> {
        self.writer
            .lock()
            .map_err(|_| Error::LockPoisoned("credential writer"))
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
        store
            .create(request("admin"), Caller::Authenticated, 500)
            .expect("bootstraps");
        assert!(
            store.enforced(),
            "the first credential turns enforcement on"
        );
        let created = store
            .create(
                NewCredential {
                    scope: "rw".into(),
                    tenant: Some("acme".into()),
                    label: Some("collector".into()),
                    ..NewCredential::default()
                },
                Caller::Authenticated,
                1_000,
            )
            .expect("creates");
        assert!(created.id.starts_with("cr_") && created.id.len() == 19);
        assert!(created.token.starts_with("trz_") && created.token.len() == 68);
        assert!(created.token[4..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(created.prefix, &created.token[..12]);

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

        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].last_used_at_ns, None, "the admin was never used");
        assert_eq!(listed[1].last_used_at_ns, Some(2_000));
        let wire = serde_json::to_string(&listed).expect("serializes");
        assert!(!wire.contains(&created.token[12..]), "{wire}");
        assert!(!wire.contains("digest"), "{wire}");
    }

    #[test]
    fn revocation_and_expiry_survive_a_reopen_and_refuse_alike() {
        let dir = Dir::new();
        let (revoked, expiring, kept) = {
            let store = CredentialStore::open(&dir.0).expect("opens");
            let revoked = store
                .create(request("admin"), Caller::Authenticated, 10)
                .expect("creates");
            let expiring = store
                .create(
                    NewCredential {
                        expires_in_seconds: Some(5),
                        ..request("ro")
                    },
                    Caller::Authenticated,
                    10,
                )
                .expect("creates");
            let kept = store
                .create(request("ro"), Caller::Authenticated, 10)
                .expect("creates");
            assert_eq!(
                store
                    .revoke(&revoked.id, Caller::Authenticated, 20)
                    .expect("revokes"),
                Revoked::At(20)
            );
            assert_eq!(
                store
                    .revoke(&revoked.id, Caller::Authenticated, 30)
                    .expect("idempotent"),
                Revoked::At(20),
                "a second revoke keeps the first instant"
            );
            assert_eq!(
                store
                    .revoke("cr_nope", Caller::Authenticated, 30)
                    .expect("ok"),
                Revoked::Unknown
            );
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
        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(listed[0].revoked_at_ns, Some(20));
        assert_eq!(listed[0].last_used_at_ns, None, "last use is memory only");
    }

    #[test]
    fn revoking_everything_locks_the_store_rather_than_opening_it() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let only = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("creates");
        store
            .revoke(&only.id, Caller::Authenticated, 2)
            .expect("revokes");
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
        store
            .create(request("admin"), Caller::Authenticated, 0)
            .expect("bootstraps");
        let supplied = "control-plane-issued-0123456789";
        let created = store
            .create(
                NewCredential {
                    token: Some(supplied.into()),
                    ..request("rw")
                },
                Caller::Authenticated,
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
            Caller::Authenticated,
            2,
        );
        assert!(
            matches!(duplicate, Err(Error::Conflict(_))),
            "{duplicate:?}"
        );
        store
            .revoke(&created.id, Caller::Authenticated, 3)
            .expect("revokes");
        let again = store.create(
            NewCredential {
                token: Some(supplied.into()),
                ..request("ro")
            },
            Caller::Authenticated,
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
                Caller::Authenticated,
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
            match store.create(bad, Caller::Authenticated, 6) {
                Err(Error::Invalid(message)) => assert!(message.contains(field), "{message}"),
                other => panic!("{field}: {other:?}"),
            }
        }
        assert_eq!(
            store.list(Caller::Authenticated).expect("lists").len(),
            2,
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

        let api = store
            .create(request("rw"), Caller::Authenticated, 9)
            .expect("creates");
        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(
            listed.iter().map(|view| view.source).collect::<Vec<_>>(),
            vec![Source::Env, Source::Env, Source::Api]
        );
        assert_eq!(listed[0].prefix, "env-", "a quarter of a 16-char token");
        assert!(matches!(
            store.revoke(&id, Caller::Authenticated, 10),
            Err(Error::Conflict(_))
        ));
        assert!(
            store
                .create(
                    NewCredential {
                        token: Some("env-reader-token-padded-out".into()),
                        ..request("rw")
                    },
                    Caller::Authenticated,
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
            Caller::Authenticated,
            14,
        );
        assert!(matches!(claimed, Err(Error::Conflict(_))), "{claimed:?}");
    }

    #[test]
    fn a_torn_final_append_is_healed_and_mid_file_damage_refuses() {
        let dir = Dir::new();
        let token = {
            let store = CredentialStore::open(&dir.0).expect("opens");
            store
                .create(request("admin"), Caller::Authenticated, 1)
                .expect("creates")
                .token
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
            .create(request("ro"), Caller::Authenticated, 3)
            .expect("appends after the heal");
        drop(store);
        let store = CredentialStore::open(&dir.0).expect("reopens cleanly");
        assert_eq!(store.list(Caller::Authenticated).expect("lists").len(), 2);
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
        let created = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("creates");
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

    /// Open mode is the store's to decide, under the lock its step takes:
    /// the first open-mode create bootstraps, and every open-mode call after
    /// it — create, revoke, list — is refused and leaves no trace.
    #[test]
    fn an_open_mode_caller_is_honoured_only_while_the_store_is_open() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        assert_eq!(store.list(Caller::Open).expect("lists"), Vec::new());
        assert_eq!(
            store.revoke("cr_nope", Caller::Open, 1).expect("answers"),
            Revoked::Unknown
        );
        let admin = store
            .create(request("admin"), Caller::Open, 2)
            .expect("bootstraps");
        let lost = store.create(
            NewCredential {
                token: Some("lost-the-race-0123456789abcdef".into()),
                ..request("admin")
            },
            Caller::Open,
            3,
        );
        assert!(matches!(lost, Err(Error::Unauthenticated)), "{lost:?}");
        assert!(matches!(
            store.revoke(&admin.id, Caller::Open, 4),
            Err(Error::Unauthenticated)
        ));
        assert!(matches!(
            store.list(Caller::Open),
            Err(Error::Unauthenticated)
        ));
        assert!(store
            .authenticate(Some("Bearer lost-the-race-0123456789abcdef"), 5)
            .is_err());
        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revoked_at_ns, None);

        // With TRAZA_TOKENS the store enforces from the start, so no
        // open-mode call is ever honoured.
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let config = AuthConfig::parse("admin:env-operator-token").expect("parses");
        store.install_env(&config, 1).expect("installs");
        assert!(matches!(
            store.create(request("admin"), Caller::Open, 2),
            Err(Error::Unauthenticated)
        ));
    }

    /// The race the rule exists for, run for real: many open-mode bootstraps
    /// at once, released together. Exactly one creates anything, however
    /// the threads interleave.
    #[test]
    fn of_racing_open_mode_bootstraps_exactly_one_creates() {
        let dir = Dir::new();
        let store = std::sync::Arc::new(CredentialStore::open(&dir.0).expect("opens"));
        let racers = 8;
        let start = std::sync::Arc::new(std::sync::Barrier::new(racers));
        let outcomes: Vec<Result<Created>> = (0..racers)
            .map(|_| {
                let store = std::sync::Arc::clone(&store);
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    store.create(request("admin"), Caller::Open, 1)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|racer| racer.join().expect("joins"))
            .collect();
        let created = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        assert_eq!(created, 1, "{outcomes:?}");
        assert!(outcomes
            .iter()
            .all(|outcome| matches!(outcome, Ok(_) | Err(Error::Unauthenticated))));
        assert_eq!(store.list(Caller::Authenticated).expect("lists").len(), 1);
    }

    #[test]
    fn the_first_credential_must_be_an_unbound_admin() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        for (scope, tenant) in [
            ("ro", None),
            ("rw", None),
            ("ro", Some("acme")),
            ("admin", Some("acme")),
        ] {
            let refused = store.create(
                NewCredential {
                    tenant: tenant.map(str::to_owned),
                    ..request(scope)
                },
                Caller::Authenticated,
                1,
            );
            match refused {
                Err(Error::Conflict(message)) => {
                    assert!(message.contains("unbound admin"), "{message}")
                }
                other => panic!("{scope}@{tenant:?}: {other:?}"),
            }
        }
        assert!(
            !store.enforced(),
            "a refused bootstrap leaves the store open"
        );
        assert!(
            !dir.0.join(LOG_NAME).exists(),
            "and writes nothing, so a later non-loopback start is still refused"
        );
        store
            .create(request("admin"), Caller::Authenticated, 2)
            .expect("bootstraps");
        store
            .create(
                NewCredential {
                    tenant: Some("acme".into()),
                    ..request("ro")
                },
                Caller::Authenticated,
                3,
            )
            .expect("after the first, anything an admin may grant");

        // Environment credentials are already "a credential": an operator who
        // configured TRAZA_TOKENS chose its admin there, and the store is not
        // locked whatever the first runtime credential is.
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let config = AuthConfig::parse("admin:env-operator-token").expect("parses");
        store.install_env(&config, 1).expect("installs");
        store
            .create(
                NewCredential {
                    tenant: Some("acme".into()),
                    ..request("rw")
                },
                Caller::Authenticated,
                2,
            )
            .expect("not the first credential");
    }

    #[test]
    fn debug_output_never_shows_a_token() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let supplied = "supplied-secret-0123456789abcdef";
        let asked = NewCredential {
            token: Some(supplied.into()),
            label: Some("ops".into()),
            ..request("admin")
        };
        let shown = format!("{asked:?}");
        assert!(!shown.contains(supplied), "{shown}");
        assert!(
            shown.contains("<redacted>") && shown.contains("ops"),
            "{shown}"
        );
        let created = store
            .create(asked, Caller::Authenticated, 1)
            .expect("creates");
        let shown = format!("{created:?}");
        assert!(!shown.contains(supplied), "{shown}");
        assert!(
            shown.contains(&created.prefix),
            "the prefix is fine: {shown}"
        );
        let minted = store
            .create(request("ro"), Caller::Authenticated, 2)
            .expect("mints");
        let shown = format!("{:?}", Ok::<_, Error>(minted.clone()));
        assert!(!shown.contains(&minted.token), "{shown}");
        assert!(
            !format!("{:?}", NewCredential::default()).contains("<redacted>"),
            "no token, nothing to redact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("creates");
        let mode = fs::metadata(dir.0.join(LOG_NAME))
            .expect("log")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    /// A full disk can stop a write part-way. Those bytes are not a record,
    /// and the next append must not land after them: a create acknowledged
    /// after the failure that no restart can read back is worse than the
    /// failure itself.
    #[test]
    fn a_torn_append_is_rolled_back_so_later_writes_stay_readable() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let admin = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("bootstraps");
        store.faults().torn_write = Some(17);
        let failed = store.create(request("rw"), Caller::Authenticated, 2);
        assert!(matches!(failed, Err(Error::Io(_))), "{failed:?}");
        assert_eq!(
            store.list(Caller::Authenticated).expect("lists").len(),
            1,
            "nothing joined"
        );
        let kept = store
            .create(request("ro"), Caller::Authenticated, 3)
            .expect("the store still writes");
        assert_eq!(
            store
                .revoke(&admin.id, Caller::Authenticated, 4)
                .expect("revokes"),
            Revoked::At(4)
        );
        drop(store);

        let store = CredentialStore::open(&dir.0).expect("every acknowledged record reads back");
        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(
            listed
                .iter()
                .map(|view| view.id.as_str())
                .collect::<Vec<_>>(),
            vec![admin.id.as_str(), kept.id.as_str()]
        );
        assert_eq!(listed[0].revoked_at_ns, Some(4));
        assert!(store.authenticate(Some(&bearer(&kept.token)), 5).is_ok());
    }

    /// A write that landed whole and then failed its fsync was reported as a
    /// failure, so it must not be on disk either: the caller retrying with
    /// the same supplied token would otherwise record that digest twice, and
    /// the log would refuse to open.
    #[test]
    fn a_failed_fsync_is_rolled_back_so_a_retry_cannot_record_a_token_twice() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let admin = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("bootstraps");
        let supplied = || NewCredential {
            token: Some("control-plane-issued-0123456789".into()),
            ..request("rw")
        };
        store.faults().sync = true;
        assert!(store.create(supplied(), Caller::Authenticated, 2).is_err());
        assert!(
            store
                .authenticate(Some("Bearer control-plane-issued-0123456789"), 2)
                .is_err(),
            "a failed create does not authenticate"
        );
        let created = store
            .create(supplied(), Caller::Authenticated, 3)
            .expect("the retry succeeds");

        // A revoke whose fsync failed is rolled back the same way: the token
        // still authenticates, exactly as the error said.
        store.faults().sync = true;
        assert!(store.revoke(&created.id, Caller::Authenticated, 4).is_err());
        assert!(store.authenticate(Some(&bearer(&created.token)), 4).is_ok());
        drop(store);

        let store = CredentialStore::open(&dir.0).expect("reopens");
        let listed = store.list(Caller::Authenticated).expect("lists");
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].id, admin.id);
        assert_eq!(listed[1].revoked_at_ns, None, "the failed revoke is gone");
        assert!(store.authenticate(Some(&bearer(&created.token)), 5).is_ok());
    }

    /// When the rollback fails too, nobody knows what the log's tail holds,
    /// so the store refuses every further change until a reopen heals and
    /// re-reads it — reads keep working throughout.
    #[test]
    fn a_failed_rollback_latches_the_store_until_a_reopen_reconciles_it() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        let admin = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("bootstraps");
        {
            let mut faults = store.faults();
            faults.torn_write = Some(9);
            faults.rollback = true;
        }
        assert!(store
            .create(request("rw"), Caller::Authenticated, 2)
            .is_err());
        let latched = store.create(request("ro"), Caller::Authenticated, 3);
        assert!(
            matches!(&latched, Err(Error::Io(error)) if error.to_string().contains("restart")),
            "{latched:?}"
        );
        assert!(
            store.revoke(&admin.id, Caller::Authenticated, 3).is_err(),
            "revokes refuse too"
        );
        assert!(store.authenticate(Some(&bearer(&admin.token)), 3).is_ok());
        assert_eq!(store.list(Caller::Authenticated).expect("lists").len(), 1);
        drop(store);

        let store = CredentialStore::open(&dir.0).expect("the reopen heals the torn tail");
        assert_eq!(store.list(Caller::Authenticated).expect("lists").len(), 1);
        store
            .create(request("ro"), Caller::Authenticated, 4)
            .expect("and writes again");
        drop(store);
        assert_eq!(
            CredentialStore::open(&dir.0)
                .expect("reopens")
                .list(Caller::Authenticated)
                .expect("lists")
                .len(),
            2
        );
    }

    /// The log's directory entry is made durable by the first append that
    /// succeeds, not by the append that happened to create the file: if
    /// that one failed, a later success must still sync the directory, or
    /// every credential it acknowledged could vanish with the file.
    #[test]
    fn a_failed_first_append_leaves_the_directory_sync_to_the_next() {
        let dir = Dir::new();
        let store = CredentialStore::open(&dir.0).expect("opens");
        store.faults().directory_sync = true;
        assert!(store
            .create(request("admin"), Caller::Authenticated, 1)
            .is_err());
        assert!(
            !store.enforced(),
            "the failed bootstrap left the store open"
        );
        assert!(!CredentialStore::has_api_credentials(&dir.0).expect("peeks"));
        store
            .create(request("admin"), Caller::Authenticated, 2)
            .expect("bootstraps");
        assert_eq!(store.faults().directory_syncs, 1);
        store
            .create(request("ro"), Caller::Authenticated, 3)
            .expect("creates");
        assert_eq!(store.faults().directory_syncs, 1, "once is enough");
    }

    /// A writer holds its mutex through the append's fsync; authentication
    /// and listing must not need it, or every request would stall behind a
    /// credential being minted. The writer is held here by hand — standing in
    /// for a slow disk — while another thread authenticates. The bound is a
    /// hang guard, not a performance claim.
    #[test]
    fn authentication_never_waits_for_a_writer() {
        let dir = Dir::new();
        let store = std::sync::Arc::new(CredentialStore::open(&dir.0).expect("opens"));
        let created = store
            .create(request("admin"), Caller::Authenticated, 1)
            .expect("creates");
        let held = store.writer().expect("writer");
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::sync::Arc::clone(&store);
        let token = created.token.clone();
        std::thread::spawn(move || {
            let verdict = reader.authenticate(Some(&bearer(&token)), 2).is_ok();
            let listed = reader
                .list(Caller::Authenticated)
                .map(|views| views.len())
                .unwrap_or(0);
            let _ = send.send((verdict, listed));
        });
        let answered = receive.recv_timeout(std::time::Duration::from_secs(30));
        drop(held);
        assert_eq!(answered, Ok((true, 1)));
    }
}
