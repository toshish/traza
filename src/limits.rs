//! Per-tenant limits an operator sets at runtime: monthly quotas, an ingest
//! pause, and a retention override.
//!
//! Flags describe a deployment; these describe a customer. A control plane
//! that sells plans needs to change a tenant's quota, pause a tenant that
//! stopped paying, or shorten one tenant's retention without restarting the
//! process every other tenant is writing to — so the document lives in the
//! store, is replaced whole by `POST /v1/admin/tenants/{tenant}/limits`, and
//! takes effect on the next batch (quotas, pause) or the next retention pass
//! (TTL).
//!
//! **Storage.** `limits.json` in the data directory, rewritten whole on every
//! change: written to a temp file, fsynced, renamed over the old one, and the
//! directory fsynced, so a crash leaves the old document or the new one and
//! never a blend. Outside generations and pins, like the credential log: a
//! restore brings back data, not last month's commercial terms.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, Store};

/// The limits file's name inside the data directory.
pub const FILE_NAME: &str = "limits.json";

/// One tenant's limits. Every field has a "no limit" value, and a tenant that
/// was never configured has exactly those.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TenantLimits {
    /// Spans the tenant may ingest per UTC calendar month; `None` is
    /// unlimited.
    #[serde(default)]
    pub monthly_spans: Option<u64>,
    /// Bytes (see [`crate::usage`] for what is counted) the tenant may
    /// ingest per UTC calendar month; `None` is unlimited.
    #[serde(default)]
    pub monthly_bytes: Option<u64>,
    /// Refuse every batch for this tenant. Reads, exports and erasure are
    /// unaffected — a paused tenant can still get its data out.
    #[serde(default)]
    pub ingest_paused: bool,
    /// Retention for this tenant, overriding `--tenant-ttl` and
    /// `--ttl-seconds`: `Some(0)` keeps everything, `None` falls back to the
    /// flags.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// Where a tenant's effective retention came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RetentionSource {
    /// The tenant's runtime limits document.
    Limits,
    /// A `--tenant-ttl` flag (or [`crate::Config::tenant_ttl_seconds`]).
    Flag,
    /// The global `--ttl-seconds` (or [`crate::Config::ttl_seconds`]).
    Global,
    /// Nothing configured: the tenant keeps everything.
    None,
}

/// A tenant's resolved retention window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveTtl {
    /// The window in seconds, or `None` when the tenant never expires (an
    /// explicit `0` anywhere in the chain resolves to `None` too).
    pub ttl_seconds: Option<u64>,
    /// Which layer decided it.
    pub source: RetentionSource,
}

/// The on-disk shape.
#[derive(Debug, Default, Deserialize, Serialize)]
struct LimitsFile {
    version: u32,
    tenants: BTreeMap<String, TenantLimits>,
}

/// Every tenant's limits, in memory, mirrored to `limits.json`.
#[derive(Debug)]
pub(crate) struct LimitsBook {
    path: PathBuf,
    /// Serializes [`LimitsBook::set`], which holds it across the file rewrite
    /// (two fsyncs and a rename). Taken before `tenants`, never after.
    writer: Mutex<()>,
    /// Leaf lock, and never held across I/O: every ingest batch reads it (the
    /// quota check), so a limits write must not make ingest wait on a disk
    /// flush — on macOS `sync_all` is a full device flush.
    tenants: Mutex<BTreeMap<String, TenantLimits>>,
}

impl LimitsBook {
    /// Loads `limits.json`, or starts empty when there is none. A file that
    /// does not parse refuses the open: running with no quotas because the
    /// quota file was damaged would be a silent change of commercial terms.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(FILE_NAME);
        let tenants = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<LimitsFile>(&bytes)
                    .map_err(|error| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} does not parse ({error}); move it aside to start with no \
                                 tenant limits",
                                path.display()
                            ),
                        ))
                    })?
                    .tenants
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            writer: Mutex::new(()),
            tenants: Mutex::new(tenants),
        })
    }

    /// The tenant's limits; defaults when never set.
    pub(crate) fn get(&self, tenant: &str) -> Result<TenantLimits> {
        Ok(self.lock()?.get(tenant).cloned().unwrap_or_default())
    }

    /// Every tenant that has a limits document.
    pub(crate) fn all(&self) -> Result<BTreeMap<String, TenantLimits>> {
        Ok(self.lock()?.clone())
    }

    /// Replaces the tenant's document, durably, before it takes effect.
    ///
    /// The next map is built and written holding only the writer mutex; the
    /// map readers use is locked just long enough to copy it and, once the file is
    /// durable, to swap the new one in. Writers are serialized, so nothing
    /// changes the map between the copy and the swap.
    pub(crate) fn set(&self, tenant: &str, limits: TenantLimits) -> Result<()> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| Error::LockPoisoned("tenant limits writer"))?;
        let mut next = self.lock()?.clone();
        next.insert(tenant.to_owned(), limits);
        let file = LimitsFile {
            version: 1,
            tenants: next,
        };
        write_atomically(&self.path, &serde_json::to_vec_pretty(&file)?)?;
        *self.lock()? = file.tenants;
        Ok(())
    }

    /// The runtime TTL overrides: tenant → seconds, for tenants whose
    /// document sets one.
    pub(crate) fn ttl_overrides(&self) -> Result<HashMap<String, u64>> {
        Ok(self
            .lock()?
            .iter()
            .filter_map(|(tenant, limits)| limits.ttl_seconds.map(|ttl| (tenant.clone(), ttl)))
            .collect())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, TenantLimits>>> {
        self.tenants
            .lock()
            .map_err(|_| Error::LockPoisoned("tenant limits"))
    }
}

/// Writes `bytes` to `path` so that a crash leaves either the old file or the
/// new one: temp file in the same directory, fsync, rename, directory fsync.
///
/// The temp is `.<name>.tmp` — dot-prefixed and `.tmp`-suffixed, which is
/// exactly what the store's orphan sweep removes at open, so one a crash
/// stranded does not accumulate. Callers serialize their own writes: two
/// concurrent rewrites of one file would share the temp.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = directory.join(format!(".{name}.tmp"));
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    crate::sync_directory(directory)
}

impl Store {
    /// Replaces `tenant`'s limits document (the empty string is the default
    /// tenant). Durable before it returns; quotas and the pause apply from
    /// the next batch, retention from the next expiry pass.
    pub fn set_tenant_limits(&self, tenant: &str, limits: TenantLimits) -> Result<TenantLimits> {
        if !tenant.is_empty() && !crate::valid_tenant(tenant) {
            return Err(Error::Invalid(
                "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes".to_owned(),
            ));
        }
        self.platform.limits.set(tenant, limits.clone())?;
        Ok(limits)
    }

    /// `tenant`'s limits document, or the defaults (no limits) when none was
    /// ever set.
    pub fn tenant_limits(&self, tenant: &str) -> Result<TenantLimits> {
        self.platform.limits.get(tenant)
    }

    /// The retention window that applies to `tenant` right now, and which
    /// layer it came from: the runtime limits document, else a per-tenant
    /// flag, else the global TTL, else none.
    pub fn effective_ttl(&self, tenant: &str) -> Result<EffectiveTtl> {
        let resolve = |seconds: u64, source| EffectiveTtl {
            ttl_seconds: (seconds > 0).then_some(seconds),
            source,
        };
        if let Some(seconds) = self.platform.limits.get(tenant)?.ttl_seconds {
            return Ok(resolve(seconds, RetentionSource::Limits));
        }
        if let Some(&seconds) = self.config.tenant_ttl_seconds.get(tenant) {
            return Ok(resolve(seconds, RetentionSource::Flag));
        }
        Ok(match self.config.ttl_seconds {
            Some(seconds) => resolve(seconds, RetentionSource::Global),
            None => EffectiveTtl {
                ttl_seconds: None,
                source: RetentionSource::None,
            },
        })
    }

    /// The per-tenant retention map expiry runs on: the configured overrides
    /// with every runtime override laid over them.
    pub(crate) fn tenant_ttl_policy(&self) -> Result<HashMap<String, u64>> {
        let mut policy = self.config.tenant_ttl_seconds.clone();
        policy.extend(self.platform.limits.ttl_overrides()?);
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "traza-limits-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("dir");
        path
    }

    #[test]
    fn documents_persist_and_unset_tenants_read_as_defaults() {
        let path = dir();
        {
            let book = LimitsBook::open(&path).expect("opens");
            assert_eq!(book.get("acme").expect("gets"), TenantLimits::default());
            book.set(
                "acme",
                TenantLimits {
                    monthly_spans: Some(1_000),
                    ingest_paused: true,
                    ttl_seconds: Some(0),
                    ..TenantLimits::default()
                },
            )
            .expect("sets");
            book.set("", TenantLimits::default())
                .expect("default tenant");
        }
        let book = LimitsBook::open(&path).expect("reopens");
        let acme = book.get("acme").expect("gets");
        assert_eq!(acme.monthly_spans, Some(1_000));
        assert!(acme.ingest_paused);
        assert_eq!(
            book.ttl_overrides().expect("overrides"),
            HashMap::from([("acme".to_owned(), 0)])
        );
        assert_eq!(book.all().expect("all").len(), 2);
        assert!(
            !path.join(".limits.json.tmp").exists(),
            "the temp is renamed away"
        );
        let _ = fs::remove_dir_all(&path);
    }

    /// Every ingest batch reads the book, so a write in progress — held here
    /// by hand, standing in for a slow fsync — must not block a read. The
    /// bound is a hang guard, not a performance claim.
    #[test]
    fn reads_never_wait_for_a_write_in_progress() {
        let path = dir();
        let book = std::sync::Arc::new(LimitsBook::open(&path).expect("opens"));
        book.set(
            "acme",
            TenantLimits {
                monthly_spans: Some(7),
                ..TenantLimits::default()
            },
        )
        .expect("sets");
        let held = book.writer.lock().expect("writer");
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::sync::Arc::clone(&book);
        std::thread::spawn(move || {
            let _ = send.send(reader.get("acme").map(|limits| limits.monthly_spans).ok());
        });
        let answered = receive.recv_timeout(std::time::Duration::from_secs(30));
        drop(held);
        assert_eq!(answered, Ok(Some(Some(7))));
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_damaged_file_refuses_rather_than_dropping_every_quota() {
        let path = dir();
        fs::write(path.join(FILE_NAME), b"{\"version\":1,\"tenants\":").expect("damage");
        let refused = LimitsBook::open(&path).expect_err("refuses");
        assert!(refused.to_string().contains("move it aside"), "{refused}");
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn the_wire_document_rejects_unknown_fields_and_defaults_missing_ones() {
        let parsed: TenantLimits = serde_json::from_str(r#"{"monthly_spans":5}"#).expect("parses");
        assert_eq!(
            parsed,
            TenantLimits {
                monthly_spans: Some(5),
                ..TenantLimits::default()
            }
        );
        assert!(serde_json::from_str::<TenantLimits>(r#"{"monthly_span":5}"#).is_err());
        assert_eq!(
            serde_json::to_value(TenantLimits::default()).expect("serializes"),
            serde_json::json!({"monthly_spans": null, "monthly_bytes": null,
                               "ingest_paused": false, "ttl_seconds": null})
        );
    }
}
