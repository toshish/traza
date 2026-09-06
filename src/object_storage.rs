//! Immutable snapshot archives on S3-compatible object storage (preview).
//!
//! This module publishes a **verified pin** — the same self-describing file
//! set `Store::pin_generation` produces and `/v1/backups/<label>` serves —
//! to an object store as one **immutable, explicitly named snapshot**, and
//! then reads it back three ways: ranged queries without downloading
//! segments, a verified full restore into a fresh directory, and snapshot
//! administration (list, inspect, verify, delete, cleanup).
//!
//! # What this is, and is not
//!
//! An archive here is **explicitly historical immutable state**. The live
//! writer stays local and unchanged; nothing in this module tiers active
//! data, acknowledges writes remotely, or fails a writer over. Publishing a
//! snapshot does not shrink the local store — what it saves is the disk a
//! *local archive copy* would have cost, because the archive is queryable in
//! place by range reads.
//!
//! Deletion boundaries are equally explicit. A snapshot is an independent
//! copy: local TTL expiry and local erasures do **not** reach into archives
//! already published, so retiring archived data means deleting the snapshots
//! that hold it (`delete_snapshot`), and remote bucket versioning or object
//! lock can keep bytes alive after even that — see
//! `docs/operations/object-storage.md`. Publication refuses a pin whose
//! tombstone log records a *pending* erasure, because such a pin still
//! carries the subject's bytes.
//!
//! # Trust model
//!
//! The remote is never trusted about content *relative to the manifest*.
//! Every published object is fully read back and checksum-verified before
//! the manifest that names it is written; the manifest is written **last**,
//! with a conditional create so two publishers of one snapshot id cannot
//! interleave; every remote range read is verified against the manifest's
//! per-chunk SHA-256 table (including cache hits); the manifest itself is
//! bound to the generation manifest it archived before any query is
//! served; and a remote segment open runs exactly the same
//! header/section/timestamp validation a local open runs, because both
//! share one implementation ([`crate::segment::Segment::open_from_source`]).
//! An S3 ETag is never treated as a content checksum.
//!
//! Stated precisely: **the manifest is the trust root, and it lives in the
//! bucket**, protected by credentials, TLS and the bucket's write control.
//! A party able to rewrite the manifest AND every object it names could
//! rewrite history undetected — this is not Byzantine-proof storage. The
//! defense is external: retain [`PublishReceipt::manifest_sha256`] outside
//! the bucket and enforce it through
//! [`RemoteOptions::expected_manifest_sha256`]. Deletion is fenced by
//! permanent per-snapshot tombstones (ids are never reusable), and every
//! operation checks the store identity recorded in what it reads.
//!
//! [`object_store`] owns HTTP, TLS (verifying by default) and SigV4;
//! credentials come from the environment and are never held or printed by
//! this module. A plain-HTTP endpoint is honoured only when explicitly
//! opted into, for local testing backends.
//!
//! # Memory truth
//!
//! Three residencies, reported by [`RemoteSnapshot::read_stats`]:
//! the raw **chunk cache** is globally bounded across all of a snapshot's
//! segments and evicts; the **decoded metadata indexes** of every opened
//! segment are eager and NOT bounded by that cache (exactly as they are for
//! a local open — resident cost scales with index cardinality); and each
//! segment's small decoded-block cache is bounded per segment as ever.
//! Nothing hydrates whole objects to local disk.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The vendored transport this module drives, re-exported so callers (and
/// this crate's own tests) can inject backends — [`object_store::memory::InMemory`],
/// or any custom [`object_store::ObjectStore`] — without adding their own
/// dependency on a version that must match ours exactly.
pub use object_store;

use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt};

mod admin;
mod manifest;
mod publish;
mod restore;
mod runtime;
mod snapshot;
pub mod testing;

pub use admin::{DeleteReceipt, SnapshotSummary, VerifyReceipt};
pub use manifest::{RemoteFile, RemoteManifest, RemoteObject};
pub use publish::PublishReceipt;
pub use restore::RestoreReceipt;
pub use snapshot::{ReadStats, RemoteSnapshot};

/// Bytes of one verification chunk: every object carries a SHA-256 per
/// `chunk_bytes` in its manifest entry, and every ranged read fetches and
/// verifies whole chunks. One mebibyte balances manifest size (32 raw bytes
/// of hash per MiB of data) against over-fetch on small reads, which the
/// bounded cache amortizes.
pub const CHUNK_BYTES: u32 = 1 << 20;
/// Target size of one pack object. Small files are concatenated into packs
/// up to this bound; a file larger than the bound becomes an object of its
/// own (uploaded multipart), so the bound is on packing overhead, not on
/// file size.
pub const PACK_TARGET_BYTES: u64 = 8 << 20;
/// Multipart part size for oversized single-file objects. A whole multiple
/// of [`CHUNK_BYTES`], so chunk boundaries never straddle parts.
pub(crate) const MULTIPART_PART_BYTES: usize = 8 << 20;
/// Ceiling on a remote manifest's serialized size, enforced on read before
/// the body is parsed and on write before it is uploaded.
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 << 20;
/// Ceiling on the files one snapshot may name.
pub(crate) const MAX_FILES: usize = 200_000;
/// Ceiling on the objects one snapshot may own.
pub(crate) const MAX_OBJECTS: usize = 200_000;
/// Remote layout component: snapshots live under `<prefix>/snapshots/<id>/`.
pub(crate) const SNAPSHOTS_DIR: &str = "snapshots";
/// The manifest object inside a snapshot — written LAST, conditionally.
pub(crate) const MANIFEST_OBJECT: &str = "manifest.json";
/// The durable upload-intent marker, created (conditionally) before any
/// object bytes and removed after the manifest lands. Its presence without a
/// manifest is what identifies an abandoned publication to `cleanup`.
pub(crate) const INTENT_OBJECT: &str = "UPLOADING";
/// The permanent deletion tombstone. Written FIRST by `delete_snapshot` and
/// never removed: a tombstoned snapshot id is dead forever — readers refuse
/// it, publishers refuse to claim it — which is what makes deletion safe
/// against a concurrent or resumed publisher without a distributed lock.
pub(crate) const TOMBSTONE_OBJECT: &str = "TOMBSTONE";
/// Pack objects live under `<snapshot>/objects/`.
pub(crate) const OBJECTS_DIR: &str = "objects";
/// Ceiling on one ranged read. Chunked reads never need more than one chunk
/// (≤ 16 MiB by manifest validation); anything larger is a caller bug or a
/// hostile length, refused before a hostile backend's response is collected.
pub(crate) const MAX_RANGE_BYTES: u64 = 32 << 20;
/// Ceiling on a control object (intent marker, tombstone) read.
pub(crate) const MAX_CONTROL_BYTES: usize = 64 << 10;
/// Ceiling on an inventory listing: a snapshot owns at most [`MAX_OBJECTS`]
/// packs plus its control objects, so a listing past this is a foreign or
/// runaway prefix and is refused rather than accumulated.
pub(crate) const MAX_LIST_ENTRIES: usize = MAX_OBJECTS + 16;
/// Floor on assumed transfer throughput when converting a byte count into
/// an overall wall-clock budget for one streamed operation: one MiB/s. Slow
/// but real links stay inside it; a stalled or dribbling transfer does not.
const MIN_TRANSFER_BYTES_PER_SEC: u64 = 1 << 20;

/// The overall wall-clock budget for one streamed transfer of about
/// `bytes_hint` bytes: the per-request bound plus the hint at the
/// [`MIN_TRANSFER_BYTES_PER_SEC`] floor. Distinct from `op_timeout`, which
/// bounds INACTIVITY (each request start and each stream chunk); this
/// bounds the whole operation so a dribbling backend cannot stretch one
/// call indefinitely by staying just under the inactivity bound.
pub(crate) fn transfer_budget(op_timeout: Duration, bytes_hint: u64) -> Duration {
    op_timeout.saturating_add(Duration::from_secs(
        bytes_hint / MIN_TRANSFER_BYTES_PER_SEC + 1,
    ))
}

/// Errors from archive operations.
///
/// Query paths on [`RemoteSnapshot`] return [`crate::Error`] instead, because
/// they run the engine's own query code; everything the archive layer itself
/// does reports here. No variant ever carries credential material — this
/// module never holds any.
#[derive(Debug)]
pub enum Error {
    /// The remote transport or service failed (network, HTTP status,
    /// timeout, throttling after retries).
    Remote(String),
    /// Remote or staged bytes failed an integrity check: a chunk, object,
    /// file or manifest that does not match its recorded digest, length or
    /// structure.
    Corrupt(String),
    /// A policy refusal: pending erasures in the source pin, an invalid
    /// snapshot id or path, a store-identity mismatch, an unverified pin.
    Refused(String),
    /// The snapshot (or an object publication requires to not exist)
    /// already exists.
    AlreadyExists(String),
    /// The named snapshot or object does not exist.
    NotFound(String),
    /// A local filesystem operation failed.
    Io(io::Error),
    /// An engine-layer error surfaced through shared code (pin loading,
    /// verification, span decoding).
    Engine(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote(detail) => write!(f, "remote operation failed: {detail}"),
            Self::Corrupt(detail) => write!(f, "archive integrity failure: {detail}"),
            Self::Refused(detail) => write!(f, "refused: {detail}"),
            Self::AlreadyExists(detail) => write!(f, "already exists: {detail}"),
            Self::NotFound(detail) => write!(f, "not found: {detail}"),
            Self::Io(error) => write!(f, "archive I/O error: {error}"),
            Self::Engine(error) => write!(f, "engine error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Engine(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Engine(error)
    }
}

impl From<object_store::Error> for Error {
    fn from(error: object_store::Error) -> Self {
        match error {
            object_store::Error::NotFound { path, .. } => Self::NotFound(path),
            object_store::Error::AlreadyExists { path, .. } => Self::AlreadyExists(path),
            // Some backends report a failed conditional create as a
            // precondition failure rather than AlreadyExists; for this
            // module's only conditional writes (create-if-absent) the two
            // mean the same thing: someone else holds the name.
            object_store::Error::Precondition { path, .. } => Self::AlreadyExists(path),
            other => Self::Remote(other.to_string()),
        }
    }
}

/// Result type for archive operations.
pub type Result<T> = std::result::Result<T, Error>;

/// S3-compatible backend parameters. **Carries no secrets by design**:
/// credentials are read by [`object_store`] from the standard environment
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
/// web-identity/container credentials or IMDS where configured), so deriving `Debug` here can never leak
/// one.
#[derive(Clone, Debug)]
pub struct S3Options {
    /// Bucket name.
    pub bucket: String,
    /// Region; `None` lets the environment (`AWS_REGION`) decide.
    pub region: Option<String>,
    /// Custom endpoint URL, overriding inherited endpoint settings.
    /// HTTPS always verifies TLS; [`Self::allow_http`] additionally permits HTTP.
    pub endpoint: Option<String>,
    /// Explicit opt-in to a plain-HTTP endpoint — local test backends only.
    /// Without it an `http://` endpoint is refused by the transport.
    pub allow_http: bool,
    /// Use path-style addressing (`endpoint/bucket/key`). With virtual-hosted
    /// addressing, a custom endpoint must already contain the bucket hostname.
    pub force_path_style: bool,
}

/// Which object store an archive talks to.
pub enum Backend {
    /// Amazon S3 or an S3-compatible service, credentials from the
    /// environment.
    S3(S3Options),
    /// A fresh in-process, in-memory store. For tests: contents die with
    /// the [`Remote`].
    InMemory,
    /// A caller-supplied store — the injection seam for fault backends and
    /// integration harnesses.
    Custom(Arc<dyn ObjectStore>),
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S3(options) => f.debug_tuple("S3").field(options).finish(),
            Self::InMemory => write!(f, "InMemory"),
            Self::Custom(_) => write!(f, "Custom(..)"),
        }
    }
}

/// How to reach and bound an archive. See [`Remote::open`].
#[derive(Debug)]
pub struct RemoteOptions {
    /// The backend.
    pub backend: Backend,
    /// Key prefix all snapshots live under, `""` for the bucket root.
    /// Validated like any archive path: no `..`, no empty components.
    pub prefix: String,
    /// The store identity recorded in every manifest this archive writes and
    /// checked against every manifest it reads. Two stores sharing a prefix
    /// with different identities cannot silently read each other's
    /// snapshots. Empty skips the read-side check (accept any).
    pub store_identity: String,
    /// Outer bound on each remote request (and on each streamed chunk of a
    /// long transfer). Applied by this module on top of whatever the
    /// backend's own client timeouts are, so even a custom backend is
    /// bounded.
    pub op_timeout: Duration,
    /// Retry budget handed to the S3 client. In-memory and custom backends
    /// get no retries from this module — their faults should surface.
    pub max_retries: usize,
    /// Global budget for the raw chunk cache shared by every segment of a
    /// [`RemoteSnapshot`] opened through this remote. Evicts LRU.
    pub cache_bytes: usize,
    /// Optional externally retained manifest digest: when set, every
    /// manifest this remote reads must hash to exactly this SHA-256
    /// (lowercase hex) or be refused. This is the defense the chunk tables
    /// cannot provide — they authenticate data against the manifest, and
    /// the manifest itself is otherwise only as trustworthy as the bucket's
    /// write control. [`PublishReceipt::manifest_sha256`] is the value to
    /// retain. Meaningful when this remote operates on ONE snapshot (the
    /// CLI's shape); listing several snapshots with it set will refuse all
    /// but the matching one.
    pub expected_manifest_sha256: Option<String>,
}

impl RemoteOptions {
    /// Options against `backend` with the defaults: no prefix, no identity
    /// check, 60 s per request, 3 retries, a 256 MiB chunk-cache budget.
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            prefix: String::new(),
            store_identity: String::new(),
            op_timeout: Duration::from_secs(60),
            max_retries: 3,
            cache_bytes: 256 << 20,
            expected_manifest_sha256: None,
        }
    }
}

/// Transfer counters shared by everything a [`Remote`] does. Monotonic;
/// receipts snapshot them.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Remote requests issued (puts, gets, heads, deletes, lists).
    pub requests: AtomicU64,
    /// Bytes fetched from the remote (ranged and full reads).
    pub fetched_bytes: AtomicU64,
    /// Bytes uploaded to the remote.
    pub uploaded_bytes: AtomicU64,
    /// Chunk fetches that went to the remote.
    pub chunk_fetches: AtomicU64,
    /// Chunk reads served from the bounded cache.
    pub cache_hits: AtomicU64,
}

/// One archive endpoint: a backend, a prefix, a store identity, and the
/// bounded synchronous adapter every operation runs through.
///
/// All operations are synchronous to the caller and sequential inside — no
/// unbounded concurrency — with every remote request bounded by
/// [`RemoteOptions::op_timeout`]. Cloning is a handle copy: every clone
/// shares one client, one runtime, and one set of counters, which is how an
/// opened [`RemoteSnapshot`] keeps its segments readable without borrowing.
#[derive(Clone)]
pub struct Remote {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    runtime: runtime::SyncRuntime,
    store: Arc<dyn ObjectStore>,
    base: ObjPath,
    identity: String,
    op_timeout: Duration,
    cache_bytes: usize,
    expected_manifest_sha256: Option<String>,
    counters: Counters,
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("base", &self.inner.base.as_ref())
            .field("identity", &self.inner.identity)
            .field("op_timeout", &self.inner.op_timeout)
            .finish_non_exhaustive()
    }
}

impl Remote {
    /// Opens an archive endpoint. Constructs the backend client (for S3:
    /// bounded timeouts and retries, TLS verifying, credentials from the
    /// environment) and the dedicated runtime; performs no remote I/O.
    pub fn open(options: RemoteOptions) -> Result<Self> {
        if !options.prefix.is_empty() && !manifest::valid_archive_path(&options.prefix) {
            return Err(Error::Refused(format!(
                "prefix {:?} is not a valid archive path",
                options.prefix
            )));
        }
        let store: Arc<dyn ObjectStore> = match options.backend {
            Backend::Custom(store) => store,
            Backend::InMemory => Arc::new(object_store::memory::InMemory::new()),
            Backend::S3(s3) => {
                use object_store::aws::AmazonS3Builder;
                use object_store::{BackoffConfig, ClientOptions, RetryConfig};
                let mut builder = AmazonS3Builder::from_env()
                    .with_bucket_name(&s3.bucket)
                    .with_virtual_hosted_style_request(!s3.force_path_style)
                    .with_retry(RetryConfig {
                        backoff: BackoffConfig::default(),
                        max_retries: options.max_retries,
                        retry_timeout: options
                            .op_timeout
                            .saturating_mul((options.max_retries as u32).saturating_add(1)),
                    })
                    // `with_client_options` REPLACES the builder's client
                    // options wholesale, and the builder-level allow_http
                    // setter merely edits those options — so the HTTP
                    // opt-in must live on the options object handed in
                    // here, or the replacement would silently clear it.
                    .with_client_options(
                        ClientOptions::new()
                            .with_allow_http(s3.allow_http)
                            .with_timeout(options.op_timeout)
                            .with_connect_timeout(Duration::from_secs(10)),
                    );
                if let Some(region) = &s3.region {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = &s3.endpoint {
                    // The service-specific environment endpoint takes
                    // precedence over `with_endpoint`; an explicit caller
                    // selection must override that inherited default too.
                    builder = builder
                        .with_config(object_store::aws::AmazonS3ConfigKey::S3Endpoint, endpoint);
                }
                Arc::new(
                    builder
                        .build()
                        .map_err(|error| Error::Remote(error.to_string()))?,
                )
            }
        };
        Ok(Self {
            inner: Arc::new(RemoteInner {
                runtime: runtime::SyncRuntime::new()?,
                store,
                base: if options.prefix.is_empty() {
                    ObjPath::default()
                } else {
                    ObjPath::from(options.prefix.as_str())
                },
                identity: options.store_identity,
                op_timeout: options.op_timeout,
                cache_bytes: options.cache_bytes,
                expected_manifest_sha256: options.expected_manifest_sha256,
                counters: Counters::default(),
            }),
        })
    }

    /// Publishes a verified local pin as snapshot `snapshot_id`: the pin is
    /// re-verified locally, checked for pending erasures (from its own
    /// tombstone log), uploaded as packed objects that are each read back
    /// and digest-verified, and only then named by a conditionally created
    /// manifest — written last, so a failure anywhere leaves no visible
    /// snapshot. See [`PublishReceipt`] and the operations guide.
    pub fn publish_pin(
        &self,
        pin_dir: &std::path::Path,
        snapshot_id: &str,
    ) -> Result<PublishReceipt> {
        publish::publish_pin(self, pin_dir, snapshot_id)
    }

    /// Opens snapshot `snapshot_id` for querying: fetches and validates its
    /// manifest, then opens every segment remotely (metadata sections only —
    /// record bytes stay remote and are range-read on demand).
    pub fn open_snapshot(&self, snapshot_id: &str) -> Result<RemoteSnapshot> {
        snapshot::open(self, snapshot_id)
    }

    /// Lists every snapshot under the prefix, complete and incomplete.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotSummary>> {
        admin::list_snapshots(self)
    }

    /// Fetches and validates one snapshot's manifest.
    pub fn inspect_snapshot(&self, snapshot_id: &str) -> Result<RemoteManifest> {
        admin::inspect(self, snapshot_id)
    }

    /// Verifies a snapshot against its manifest: every object's existence
    /// and length, and with `deep` every object's full SHA-256, streamed.
    pub fn verify_snapshot(&self, snapshot_id: &str, deep: bool) -> Result<VerifyReceipt> {
        admin::verify(self, snapshot_id, deep)
    }

    /// Deletes a PUBLISHED snapshot, permanently retiring its id: a
    /// [`TOMBSTONE_OBJECT`] is written first and never removed, the
    /// manifest goes next (visibility), then every object the snapshot
    /// owns. The tombstone is the fence: readers refuse a tombstoned id,
    /// and a publisher refuses to claim or commit one, so a concurrent or
    /// resumed publisher can never make the deleted name visible again —
    /// **snapshot ids are never reusable after deletion.**
    ///
    /// Idempotent and resumable: re-running an interrupted delete finishes
    /// the sweep. Refuses a snapshot whose manifest records a different
    /// store identity, an id with an upload in flight (no manifest, an
    /// intent marker, no tombstone — use [`Self::cleanup_snapshot`] with
    /// the quiescence acknowledgment), and never reports success while
    /// listed objects remain. Never touches anything outside
    /// `<prefix>/snapshots/<snapshot_id>/`.
    pub fn delete_snapshot(&self, snapshot_id: &str) -> Result<DeleteReceipt> {
        admin::delete(self, snapshot_id)
    }

    /// Removes the debris of an abandoned publication (intent marker, no
    /// manifest, no tombstone), or a stale intent marker beside a complete
    /// snapshot. Never deletes a complete snapshot's objects; a tombstoned
    /// id resumes the delete sweep instead.
    ///
    /// Sweeping an abandoned upload **permanently retires its id** — the
    /// tombstone is written before the sweep and never removed, so a
    /// publisher for the id that resumes later can neither claim the name
    /// nor make its swept bytes visible, and a retry must pick a fresh id.
    ///
    /// `publisher_quiescent` is the operator's explicit acknowledgment that
    /// **no publisher for this snapshot id is running anywhere** — sweeping
    /// under a live publisher races its manifest commit. Without it, a
    /// manifest-less prefix is refused. (The publisher additionally
    /// re-checks the tombstone and its own intent marker immediately before
    /// committing, and the tombstone again after, so a publisher racing or
    /// resumed after a cleanup aborts rather than publishing; the
    /// acknowledgment is what makes the sweep itself safe to start.)
    ///
    /// Physical limits, stated rather than papered over: cleanup removes
    /// LISTED objects. Parts of an incomplete multipart upload are not
    /// listable and are aborted only when the publisher's own error paths
    /// ran; after a publisher crash they linger until the bucket's
    /// `AbortIncompleteMultipartUpload` lifecycle rule reaps them —
    /// configure one (see the operations guide). The receipt counts only
    /// what was actually deleted.
    pub fn cleanup_snapshot(
        &self,
        snapshot_id: &str,
        publisher_quiescent: bool,
    ) -> Result<DeleteReceipt> {
        admin::cleanup(self, snapshot_id, publisher_quiescent)
    }

    /// Restores a snapshot into `target`, which must not exist: every file
    /// is streamed through chunk verification into a staging directory,
    /// re-verified whole against the embedded generation manifest, and the
    /// staging directory is atomically renamed into place. The result is a
    /// pin-equivalent directory `Store::restore` installs.
    pub fn restore_snapshot(
        &self,
        snapshot_id: &str,
        target: &std::path::Path,
    ) -> Result<RestoreReceipt> {
        restore::restore_snapshot(self, snapshot_id, target)
    }

    /// The identity this remote stamps into and expects from manifests.
    pub fn store_identity(&self) -> &str {
        &self.inner.identity
    }

    // ---------------------------------------------------------------- paths

    pub(crate) fn snapshot_root(&self, snapshot_id: &str) -> ObjPath {
        self.inner
            .base
            .clone()
            .join(SNAPSHOTS_DIR)
            .join(snapshot_id)
    }

    /// `<prefix>/snapshots` — where snapshot ids are enumerated.
    pub(crate) fn snapshots_prefix(&self) -> ObjPath {
        self.inner.base.clone().join(SNAPSHOTS_DIR)
    }

    pub(crate) fn manifest_path(&self, snapshot_id: &str) -> ObjPath {
        self.snapshot_root(snapshot_id).join(MANIFEST_OBJECT)
    }

    pub(crate) fn intent_path(&self, snapshot_id: &str) -> ObjPath {
        self.snapshot_root(snapshot_id).join(INTENT_OBJECT)
    }

    pub(crate) fn tombstone_path(&self, snapshot_id: &str) -> ObjPath {
        self.snapshot_root(snapshot_id).join(TOMBSTONE_OBJECT)
    }

    pub(crate) fn object_path(&self, snapshot_id: &str, name: &str) -> ObjPath {
        // `name` is a validated archive path (`objects/NNNNNNNN.pack`), so
        // joining component-wise cannot traverse.
        let mut path = self.snapshot_root(snapshot_id);
        for part in name.split('/') {
            path = path.join(part);
        }
        path
    }

    /// The externally retained manifest digest this remote requires, if any.
    pub(crate) fn expected_manifest_sha256(&self) -> Option<&str> {
        self.inner.expected_manifest_sha256.as_deref()
    }

    // ------------------------------------------------- bounded sync remote I/O

    /// Runs one bounded remote future to completion on the dedicated
    /// runtime.
    fn run<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        self.inner.runtime.run(fut)
    }

    async fn timed<T>(
        op_timeout: Duration,
        what: &'static str,
        fut: impl std::future::Future<Output = std::result::Result<T, object_store::Error>>,
    ) -> Result<T> {
        match tokio::time::timeout(op_timeout, fut).await {
            Ok(result) => result.map_err(Error::from),
            Err(_) => Err(Error::Remote(format!(
                "{what} timed out after {op_timeout:?}"
            ))),
        }
    }

    /// What remains of an overall streamed-transfer deadline, or `None` once
    /// it is spent (which includes the instant it is reached — a result that
    /// becomes ready exactly at the deadline must be treated as too late).
    ///
    /// Every streamed wait is bounded by the smaller of this and `op_timeout`
    /// so a long transfer's overall budget can never overshoot by a whole
    /// inactivity interval, and the same value re-checked after an await
    /// rejects a chunk or EOF that arrived past the deadline.
    fn remaining_budget(started: Instant, overall: Duration) -> Option<Duration> {
        match overall.checked_sub(started.elapsed()) {
            Some(remaining) if !remaining.is_zero() => Some(remaining),
            _ => None,
        }
    }

    /// HEAD: the object's length, or `None` when it does not exist.
    pub(crate) fn head_len(&self, location: &ObjPath) -> Result<Option<u64>> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let result = self.run(async move {
            Self::timed(op_timeout, "head", store.head(&location))
                .await
                .map(|meta| meta.size)
        });
        match result {
            Ok(size) => Ok(Some(size)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Ranged GET of exactly `range.end - range.start` bytes, refusing an
    /// inverted or over-[`MAX_RANGE_BYTES`] request before anything is
    /// transferred and refusing a backend that answers a different range,
    /// too short an object, or a body that overruns the request — the last
    /// caught on the FIRST oversized chunk, before its tail is ever polled,
    /// so a hostile body is never accumulated. The request and its body
    /// share one `op_timeout` budget (not a fresh one per chunk), re-checked
    /// after every awaited poll and immediately before success, because a
    /// ready result can surface after the timer's deadline.
    pub(crate) fn get_range(
        &self,
        location: &ObjPath,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        // Reject an inverted range before it reaches the backend: it can
        // only be a caller bug or a hostile length, and `end - start` would
        // otherwise saturate to zero and mask it.
        if range.end < range.start {
            return Err(Error::Corrupt(format!(
                "{}: inverted ranged read {}..{}",
                location.as_ref(),
                range.start,
                range.end
            )));
        }
        let want = range.end - range.start;
        if want > MAX_RANGE_BYTES {
            return Err(Error::Corrupt(format!(
                "{}: {want}-byte ranged read exceeds the {MAX_RANGE_BYTES} bound",
                location.as_ref()
            )));
        }
        let display_location = location.to_string();
        let requested = range.clone();
        let bytes = self.run(async move {
            use futures_util::StreamExt;
            let started = Instant::now();
            // Issue the request with an explicit range and inspect the
            // response BEFORE any body byte is collected: `get_range`'s own
            // `bytes()` would drain the whole (possibly unbounded) body
            // first, so drive the stream manually instead.
            let result = Self::timed(
                op_timeout,
                "get_range",
                store.get_opts(
                    &location,
                    object_store::GetOptions::new().with_range(Some(requested.clone())),
                ),
            )
            .await?;
            if Self::remaining_budget(started, op_timeout).is_none() {
                return Err(Error::Remote(format!(
                    "get_range exceeded its overall {op_timeout:?} budget"
                )));
            }
            if result.range != requested {
                return Err(Error::Corrupt(format!(
                    "{display_location}: ranged read answered {:?}, requested {:?}",
                    result.range, requested
                )));
            }
            let object_len = result.meta.size;
            if object_len < requested.end {
                return Err(Error::Corrupt(format!(
                    "{display_location}: object is {object_len} bytes, cannot cover \
                     range end {}",
                    requested.end
                )));
            }
            let mut stream = result.into_stream();
            let mut collected: Vec<u8> = Vec::with_capacity(want as usize);
            loop {
                let Some(wait) = Self::remaining_budget(started, op_timeout) else {
                    return Err(Error::Remote(format!(
                        "get_range exceeded its overall {op_timeout:?} budget"
                    )));
                };
                let next = match tokio::time::timeout(wait, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        return Err(Error::Remote(format!(
                            "get_range timed out after {op_timeout:?}"
                        )))
                    }
                };
                // A chunk or EOF can become ready right at the deadline;
                // re-check the shared budget after the await before trusting
                // it.
                if Self::remaining_budget(started, op_timeout).is_none() {
                    return Err(Error::Remote(format!(
                        "get_range exceeded its overall {op_timeout:?} budget"
                    )));
                }
                match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(Error::from)?;
                        // Reject the first chunk that would overrun the
                        // request outright — appending it, then polling on,
                        // is exactly the unbounded accumulation this avoids.
                        if collected.len().saturating_add(chunk.len()) as u64 > want {
                            return Err(Error::Corrupt(format!(
                                "{display_location}: ranged body overruns the {want}-byte \
                                 request"
                            )));
                        }
                        collected.extend_from_slice(&chunk);
                    }
                    None => break,
                }
            }
            if collected.len() as u64 != want {
                return Err(Error::Corrupt(format!(
                    "{display_location}: ranged read returned {} bytes, wanted {want}",
                    collected.len()
                )));
            }
            // A ready EOF can also arrive after the timer; the whole
            // request+body must have finished inside the shared budget.
            if Self::remaining_budget(started, op_timeout).is_none() {
                return Err(Error::Remote(format!(
                    "get_range exceeded its overall {op_timeout:?} budget"
                )));
            }
            Ok(collected)
        })?;
        self.inner
            .counters
            .fetched_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    /// Whole-object GET, streamed, refusing anything larger than
    /// `max_bytes` before it is held. Bounded twice: `op_timeout` is the
    /// INACTIVITY bound on the request and each stream chunk, and
    /// [`transfer_budget`] over `max_bytes` is the overall wall-clock bound
    /// a dribbling backend cannot stretch.
    pub(crate) fn get_all(&self, location: &ObjPath, max_bytes: usize) -> Result<Vec<u8>> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let overall = transfer_budget(op_timeout, max_bytes as u64);
        let bytes = self.run(async move {
            use futures_util::StreamExt;
            let started = Instant::now();
            let initial_wait = Self::remaining_budget(started, overall)
                .ok_or_else(|| {
                    Error::Remote(format!("get exceeded its overall {overall:?} budget"))
                })?
                .min(op_timeout);
            let result = Self::timed(initial_wait, "get", store.get(&location)).await?;
            let mut stream = result.into_stream();
            let mut collected: Vec<u8> = Vec::new();
            loop {
                let remaining = Self::remaining_budget(started, overall).ok_or_else(|| {
                    Error::Remote(format!("get exceeded its overall {overall:?} budget"))
                })?;
                let wait = remaining.min(op_timeout);
                let next = match tokio::time::timeout(wait, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        return Err(Error::Remote(format!(
                            "get stream timed out after {wait:?}"
                        )))
                    }
                };
                if Self::remaining_budget(started, overall).is_none() {
                    return Err(Error::Remote(format!(
                        "get exceeded its overall {overall:?} budget"
                    )));
                }
                match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(Error::from)?;
                        if collected.len().saturating_add(chunk.len()) > max_bytes {
                            return Err(Error::Corrupt(format!(
                                "{}: object exceeds the {max_bytes}-byte bound",
                                location.as_ref()
                            )));
                        }
                        collected.extend_from_slice(&chunk);
                    }
                    None => return Ok(collected),
                }
            }
        })?;
        self.inner
            .counters
            .fetched_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    /// Streams the whole object, verifying its exact length and SHA-256
    /// without holding it. Returns the byte count verified. Bounded like
    /// [`Self::get_all`]: inactivity per chunk, plus an overall
    /// [`transfer_budget`] over the expected length.
    pub(crate) fn get_verify(
        &self,
        location: &ObjPath,
        expect_len: u64,
        expect_sha256: &str,
    ) -> Result<u64> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let overall = transfer_budget(op_timeout, expect_len);
        let expect_sha256 = expect_sha256.to_owned();
        let read = self.run(async move {
            use futures_util::StreamExt;
            let started = Instant::now();
            let initial_wait = Self::remaining_budget(started, overall)
                .ok_or_else(|| {
                    Error::Remote(format!(
                        "verification exceeded its overall {overall:?} budget"
                    ))
                })?
                .min(op_timeout);
            let result = Self::timed(initial_wait, "get", store.get(&location)).await?;
            let mut stream = result.into_stream();
            let mut hasher = crate::payload::Sha256::new();
            let mut total: u64 = 0;
            loop {
                let remaining = Self::remaining_budget(started, overall).ok_or_else(|| {
                    Error::Remote(format!(
                        "verification exceeded its overall {overall:?} budget"
                    ))
                })?;
                let wait = remaining.min(op_timeout);
                let next = match tokio::time::timeout(wait, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        return Err(Error::Remote(format!(
                            "verification stream timed out after {wait:?}"
                        )))
                    }
                };
                if Self::remaining_budget(started, overall).is_none() {
                    return Err(Error::Remote(format!(
                        "verification exceeded its overall {overall:?} budget"
                    )));
                }
                match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(Error::from)?;
                        total = total.saturating_add(chunk.len() as u64);
                        if total > expect_len {
                            return Err(Error::Corrupt(format!(
                                "{}: {total}+ bytes on the remote, {expect_len} expected",
                                location.as_ref()
                            )));
                        }
                        hasher.update(&chunk);
                    }
                    None => break,
                }
            }
            if total != expect_len {
                return Err(Error::Corrupt(format!(
                    "{}: {total} bytes on the remote, {expect_len} expected",
                    location.as_ref()
                )));
            }
            if hasher.finalize_hex() != expect_sha256 {
                return Err(Error::Corrupt(format!(
                    "{}: digest mismatch on read-back",
                    location.as_ref()
                )));
            }
            if Self::remaining_budget(started, overall).is_none() {
                return Err(Error::Remote(format!(
                    "verification exceeded its overall {overall:?} budget"
                )));
            }
            Ok(total)
        })?;
        self.inner
            .counters
            .fetched_bytes
            .fetch_add(read, Ordering::Relaxed);
        Ok(read)
    }

    /// Unconditional PUT.
    pub(crate) fn put(&self, location: &ObjPath, bytes: Vec<u8>) -> Result<()> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        self.inner
            .counters
            .uploaded_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        self.run(async move {
            Self::timed(op_timeout, "put", store.put(&location, bytes.into()))
                .await
                .map(|_| ())
        })
    }

    /// Conditional create: `Ok(true)` when this call created the object,
    /// `Ok(false)` when it already existed. This is what makes same-name
    /// publication single-winner on backends with conditional writes
    /// (S3 `If-None-Match`, and the in-memory store).
    pub(crate) fn put_create(&self, location: &ObjPath, bytes: Vec<u8>) -> Result<bool> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        self.inner
            .counters
            .uploaded_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let result = self.run(async move {
            use object_store::{PutMode, PutOptions};
            Self::timed(
                op_timeout,
                "put",
                store.put_opts(
                    &location,
                    bytes.into(),
                    PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                ),
            )
            .await
            .map(|_| ())
        });
        match result {
            Ok(()) => Ok(true),
            Err(Error::AlreadyExists(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Multipart upload of a local file as one object, streamed in
    /// [`MULTIPART_PART_BYTES`] parts, hashing the whole object and each
    /// [`CHUNK_BYTES`] chunk as it goes. Returns
    /// `(object sha256, chunk sha256s, bytes uploaded)`.
    pub(crate) fn put_multipart_file(
        &self,
        location: &ObjPath,
        local: &std::path::Path,
        expect_len: u64,
    ) -> Result<(String, Vec<String>, u64)> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let local = local.to_path_buf();
        let uploaded = self.run(async move {
            use std::io::Read;
            let mut upload =
                Self::timed(op_timeout, "put_multipart", store.put_multipart(&location)).await?;
            let outcome = async {
                let mut file = std::fs::File::open(&local)?;
                let mut whole = crate::payload::Sha256::new();
                let mut chunks: Vec<String> = Vec::new();
                let mut total: u64 = 0;
                let mut part = vec![0u8; MULTIPART_PART_BYTES];
                loop {
                    // Fill one part (short only at EOF).
                    let mut filled = 0usize;
                    while filled < part.len() {
                        let read = file.read(&mut part[filled..])?;
                        if read == 0 {
                            break;
                        }
                        filled += read;
                    }
                    if filled == 0 {
                        break;
                    }
                    total = total.saturating_add(filled as u64);
                    if total > expect_len {
                        return Err(Error::Corrupt(format!(
                            "{}: grew past its manifested {expect_len} bytes mid-upload",
                            local.display()
                        )));
                    }
                    whole.update(&part[..filled]);
                    for chunk in part[..filled].chunks(CHUNK_BYTES as usize) {
                        chunks.push(crate::payload::sha256_hex(chunk));
                    }
                    let payload: Vec<u8> = part[..filled].to_vec();
                    match tokio::time::timeout(op_timeout, upload.put_part(payload.into())).await {
                        Ok(result) => result.map_err(Error::from)?,
                        Err(_) => {
                            return Err(Error::Remote(format!(
                                "put_part timed out after {op_timeout:?}"
                            )))
                        }
                    }
                    if filled < part.len() {
                        break;
                    }
                }
                if total != expect_len {
                    return Err(Error::Corrupt(format!(
                        "{}: {total} bytes read, {expect_len} manifested",
                        local.display()
                    )));
                }
                match tokio::time::timeout(op_timeout, upload.complete()).await {
                    Ok(result) => result.map_err(Error::from)?,
                    Err(_) => {
                        return Err(Error::Remote(format!(
                            "multipart complete timed out after {op_timeout:?}"
                        )))
                    }
                };
                Ok((whole.finalize_hex(), chunks, total))
            }
            .await;
            if outcome.is_err() {
                // Best effort: an aborted multipart leaves nothing for a
                // lifecycle rule to reap. The scripted-fault and crash cases
                // are why `cleanup` exists regardless.
                let _ = tokio::time::timeout(op_timeout, upload.abort()).await;
            }
            outcome
        })?;
        self.inner
            .counters
            .uploaded_bytes
            .fetch_add(uploaded.2, Ordering::Relaxed);
        Ok(uploaded)
    }

    /// DELETE, tolerating absence — deletes are idempotent by contract.
    pub(crate) fn delete(&self, location: &ObjPath) -> Result<()> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let location = location.clone();
        let op_timeout = self.inner.op_timeout;
        let result = self
            .run(async move { Self::timed(op_timeout, "delete", store.delete(&location)).await });
        match result {
            Ok(()) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Every object under `prefix`, as `(path, bytes)`. Capped at
    /// [`MAX_LIST_ENTRIES`] — a snapshot's inventory is bounded by
    /// construction, so an unbounded listing is a foreign or runaway prefix
    /// — and bounded overall as well as per page.
    pub(crate) fn list_all(&self, prefix: &ObjPath) -> Result<Vec<(ObjPath, u64)>> {
        self.inner.counters.requests.fetch_add(1, Ordering::Relaxed);
        let store = Arc::clone(&self.inner.store);
        let prefix = prefix.clone();
        let op_timeout = self.inner.op_timeout;
        let overall = op_timeout.saturating_mul(16);
        self.run(async move {
            use futures_util::StreamExt;
            let started = Instant::now();
            let mut stream = store.list(Some(&prefix));
            let mut found = Vec::new();
            loop {
                let remaining = Self::remaining_budget(started, overall).ok_or_else(|| {
                    Error::Remote(format!("list exceeded its overall {overall:?} budget"))
                })?;
                let wait = remaining.min(op_timeout);
                let next = match tokio::time::timeout(wait, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => return Err(Error::Remote(format!("list timed out after {wait:?}"))),
                };
                if Self::remaining_budget(started, overall).is_none() {
                    return Err(Error::Remote(format!(
                        "list exceeded its overall {overall:?} budget"
                    )));
                }
                match next {
                    Some(meta) => {
                        let meta = meta.map_err(Error::from)?;
                        if found.len() >= MAX_LIST_ENTRIES {
                            return Err(Error::Remote(format!(
                                "{}: more than {MAX_LIST_ENTRIES} objects under one \
                                 snapshot prefix; refusing to inventory it",
                                prefix.as_ref()
                            )));
                        }
                        let size = meta.size;
                        found.push((meta.location, size));
                    }
                    None => return Ok(found),
                }
            }
        })
    }

    /// Reads and parses one small control object (intent marker,
    /// tombstone), or `None` when absent. Bounded at
    /// [`MAX_CONTROL_BYTES`]; unparseable contents are an error, never a
    /// guess — these objects gate deletion and publication.
    pub(crate) fn read_control(&self, location: &ObjPath) -> Result<Option<serde_json::Value>> {
        match self.get_all(location, MAX_CONTROL_BYTES) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                Error::Corrupt(format!(
                    "{}: control object does not parse: {error}",
                    location.as_ref()
                ))
            }),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// A configured identity requires an exact recorded match. Missing,
    /// empty or wrong-typed identities (`None`) cannot bypass this gate.
    /// An empty configured identity retains the explicit accept-any mode.
    pub(crate) fn check_identity(&self, recorded: Option<&str>, what: &str) -> Result<()> {
        let ours = self.store_identity();
        if ours.is_empty() {
            return Ok(());
        }
        match recorded {
            Some(theirs) if theirs == ours => Ok(()),
            Some(theirs) if !theirs.is_empty() => Err(Error::Refused(format!(
                "{what} belongs to store {theirs:?}, not {ours:?}; refusing to operate \
                 across store identities"
            ))),
            _ => Err(Error::Refused(format!(
                "{what} records no store identity to match {ours:?}; refusing to operate \
                 without ownership evidence"
            ))),
        }
    }

    /// The immediate child "directories" under `prefix` (their final path
    /// component) — how snapshot ids are enumerated.
    pub(crate) fn list_child_dirs(&self, prefix: &ObjPath) -> Result<Vec<String>> {
        // The delimiter API collects its entire response before returning.
        // Derive directories from the bounded streaming inventory instead;
        // the entry cap applies to all objects under this archive prefix.
        let started = Instant::now();
        let overall = self.inner.op_timeout.saturating_mul(16);
        let mut found = std::collections::BTreeSet::new();
        for (location, _) in self.list_all(prefix)? {
            if Self::remaining_budget(started, overall).is_none() {
                return Err(Error::Remote(format!(
                    "directory listing exceeded its overall {overall:?} budget"
                )));
            }
            if let Some(mut parts) = location.prefix_match(prefix) {
                if let (Some(child), Some(_)) = (parts.next(), parts.next()) {
                    found.insert(child.as_ref().to_owned());
                }
            }
        }
        let result = found.into_iter().collect();
        if Self::remaining_budget(started, overall).is_none() {
            return Err(Error::Remote(format!(
                "directory listing exceeded its overall {overall:?} budget"
            )));
        }
        Ok(result)
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.inner.counters
    }

    pub(crate) fn cache_budget(&self) -> usize {
        self.inner.cache_bytes
    }
}

/// Whether `id` may name a snapshot: `[a-z0-9]` first, then up to 127 more
/// of `[a-z0-9._-]`, with no `..` anywhere. Explicit, immutable names only —
/// there is deliberately no `LATEST`.
pub fn valid_snapshot_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if bytes.len() > 128 || id.contains("..") {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_budgets_scale_with_the_expected_bytes() {
        let op = Duration::from_secs(10);
        assert!(transfer_budget(op, 0) >= op);
        // 64 MiB at the 1 MiB/s floor: at least a minute on top of the
        // per-request bound, and monotone in the hint.
        assert!(transfer_budget(op, 64 << 20) >= op + Duration::from_secs(64));
        assert!(transfer_budget(op, 1 << 30) > transfer_budget(op, 64 << 20));
    }

    #[test]
    fn snapshot_ids_are_strict() {
        assert!(valid_snapshot_id("nightly-2026-09-06"));
        assert!(valid_snapshot_id("a"));
        assert!(!valid_snapshot_id(""));
        assert!(!valid_snapshot_id("Nightly"));
        assert!(!valid_snapshot_id("-leading"));
        assert!(!valid_snapshot_id(".hidden"));
        assert!(!valid_snapshot_id("has/slash"));
        assert!(!valid_snapshot_id("dot..dot"));
        assert!(!valid_snapshot_id(&"x".repeat(129)));
    }
}
