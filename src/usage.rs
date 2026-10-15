//! Per-tenant usage counters, and the quota check that reads them.
//!
//! Two things a hosted deployment needs that [`crate::Store::tenant_usage`]
//! cannot give it: what a tenant has SENT (not what it holds now — expiry and
//! erasure shrink that), and an answer in O(tenants) that can sit on the
//! ingest path. So the store keeps monotonic counters, incremented as batches
//! are acknowledged:
//!
//! - `lifetime {spans, bytes, batches, rejected}` — since the counters began;
//! - `month {window, spans, bytes}` — the current UTC calendar month, reset
//!   when the month turns.
//!
//! `bytes` is each admitted span's JSON size after tenant stamping and BEFORE
//! payload offload — what the client sent, in Traza's native encoding (an OTLP
//! protobuf batch is charged its native-JSON size, not its wire size). It is
//! measured during admission ([`crate::caps`]) because by the time a batch is
//! acknowledged its large values have already become payload references.
//! `rejected` counts spans refused by a quota or a pause, attributed to each
//! refused span's own tenant.
//!
//! **Enforcement** runs in admission, before the write-ahead log append: a
//! paused tenant's batch is refused, and so is a batch that would take the
//! month's spans or bytes past the tenant's limit — the WHOLE batch, never a
//! prefix. The check reads the counters as they stand, without reserving
//! anything, so batches admitted concurrently can together overshoot a limit
//! by at most what was in flight; the next batch is then refused.
//!
//! **Persistence** is `usage.json` (atomic rewrite + fsync), written by
//! [`crate::Store::persist_usage`] — on the server's 5-second maintenance tick
//! and on every flush — and only when something changed. A crash loses the
//! counts since the last write: an UNDERCOUNT, never an overcount, bounded by
//! one tick. Outside generations and pins: a restore brings back data, it does
//! not wind back what a tenant has already been charged for.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::caps::{key_hash, Admitting};
use crate::limits::{write_atomically, LimitsBook, TenantLimits};
use crate::{Error, Result, Span, Store};

/// The usage file's name inside the data directory.
pub const FILE_NAME: &str = "usage.json";

/// Counters since the store began counting.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Lifetime {
    /// Spans admitted.
    pub spans: u64,
    /// Bytes admitted (see the module docs for what is counted).
    pub bytes: u64,
    /// Batches that admitted at least one of this tenant's spans.
    pub batches: u64,
    /// Spans refused by a quota or an ingest pause.
    pub rejected: u64,
}

/// Counters for one UTC calendar month.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Month {
    /// `YYYY-MM`.
    pub window: String,
    /// Spans admitted in the window.
    pub spans: u64,
    /// Bytes admitted in the window.
    pub bytes: u64,
}

/// Where a tenant stands against its limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaState {
    /// Under 80% of every monthly limit, or no limits at all.
    Ok,
    /// At or past 80% of at least one monthly limit.
    Warning,
    /// At or past a monthly limit: the next span is refused.
    Capped,
    /// Ingest paused by the operator.
    Paused,
}

/// One tenant's usage row, as `GET /v1/usage` serves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsageRow {
    /// The tenant; empty is the default tenant.
    pub tenant: String,
    /// Counters since the store began counting.
    pub lifetime: Lifetime,
    /// The current month's counters (zero when nothing was admitted in it).
    pub month: Month,
    /// The tenant's limits document (defaults when never set).
    pub limits: TenantLimits,
    /// Where the tenant stands against those limits.
    pub state: QuotaState,
}

/// `GET /v1/usage`'s body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsageReport {
    /// When the rows were read.
    pub as_of_ns: u64,
    /// One row per tenant, ordered by tenant.
    pub tenants: Vec<UsageRow>,
}

/// Which limit refused a batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDimension {
    /// `monthly_spans`.
    MonthlySpans,
    /// `monthly_bytes`.
    MonthlyBytes,
    /// `ingest_paused`.
    Paused,
}

impl QuotaDimension {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MonthlySpans => "monthly_spans",
            Self::MonthlyBytes => "monthly_bytes",
            Self::Paused => "paused",
        }
    }
}

/// Why a batch was refused by a quota or a pause. Carried by
/// [`crate::Error::QuotaExceeded`]; HTTP answers it `429`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaRefusal {
    /// The tenant whose limit refused the batch.
    pub tenant: String,
    /// Which limit.
    pub dimension: QuotaDimension,
    /// The limit's value; `None` for a pause.
    pub limit: Option<u64>,
    /// The month's usage in that dimension before this batch (spans, for a
    /// pause).
    pub used: u64,
    /// The month the usage belongs to, `YYYY-MM`.
    pub window: String,
    /// When the window resets, for a monthly limit: the first instant of
    /// next month, UTC. `None` for a pause, which lifts when the operator
    /// lifts it.
    pub resets_at_ns: Option<u64>,
    /// Seconds a client should wait before retrying: until the reset for a
    /// monthly limit (rounded up), 60 for a pause.
    pub retry_after_seconds: u64,
}

impl QuotaRefusal {
    /// The `429` body: `{"error": .., "quota": {..}}`.
    pub fn to_json(&self) -> Value {
        json!({
            "error": self.to_string(),
            "quota": {
                "tenant": self.tenant,
                "dimension": self.dimension.as_str(),
                "limit": self.limit,
                "used": self.used,
                "window": self.window,
                "resets_at": self.resets_at_ns.map(crate::mcp::rfc3339),
            },
        })
    }
}

impl fmt::Display for QuotaRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tenant.as_str() {
            "" => write!(
                f,
                "quota exceeded: {} for the default tenant",
                self.dimension.as_str()
            ),
            tenant => write!(
                f,
                "quota exceeded: {} for tenant {tenant}",
                self.dimension.as_str()
            ),
        }
    }
}

/// Retry-After for a pause: long enough not to hammer, short enough that a
/// lifted pause is noticed promptly.
const PAUSE_RETRY_SECONDS: u64 = 60;

/// One tenant's counters, as persisted.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Counters {
    lifetime: Lifetime,
    month: Month,
}

impl Counters {
    /// The month counters as of `window`: these, or zeros if they belong to
    /// an earlier month.
    fn month_in(&self, window: &str) -> Month {
        if self.month.window == window {
            self.month.clone()
        } else {
            Month {
                window: window.to_owned(),
                spans: 0,
                bytes: 0,
            }
        }
    }

    fn roll(&mut self, window: &str) {
        if self.month.window != window {
            self.month = Month {
                window: window.to_owned(),
                spans: 0,
                bytes: 0,
            };
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UsageFile {
    version: u32,
    tenants: BTreeMap<String, Counters>,
}

#[derive(Debug, Default)]
struct Inner {
    tenants: BTreeMap<String, Counters>,
    /// Bumped on every change; compared with `persisted` so an idle store
    /// does not rewrite and fsync the file every tick.
    version: u64,
    persisted: u64,
}

/// Every tenant's counters, in memory, mirrored to `usage.json`.
#[derive(Debug)]
pub(crate) struct UsageBook {
    path: PathBuf,
    /// Leaf lock, held for O(tenants in the batch) on the ingest path and
    /// never across I/O.
    inner: Mutex<Inner>,
    /// Serializes [`UsageBook::persist`], whose rewrite shares one temp file.
    persist: Mutex<()>,
}

impl UsageBook {
    /// Loads `usage.json`, or starts at zero when there is none. A file that
    /// does not parse refuses the open: restarting every tenant's month at
    /// zero because the counter file was damaged would hand every quota back.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(FILE_NAME);
        let tenants = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<UsageFile>(&bytes)
                    .map_err(|error| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} does not parse ({error}); move it aside to restart every \
                                 tenant's usage counters at zero",
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
            inner: Mutex::new(Inner {
                tenants,
                version: 0,
                persisted: 0,
            }),
            persist: Mutex::new(()),
        })
    }

    /// Refuses `spans` when a tenant in it is paused or would go past a
    /// monthly limit. On refusal every span in the batch is counted as
    /// rejected under its own tenant.
    pub(crate) fn check(
        &self,
        limits: &LimitsBook,
        spans: &[Span],
        admitting: &Admitting,
        now_ns: u64,
    ) -> Result<()> {
        let tally = tally(spans.iter().map(|span| span.tenant.as_str()), admitting);
        // Limits first, under their own lock: the two locks are never held
        // together.
        let mut limited = Vec::new();
        for tenant in tally.keys() {
            let doc = limits.get(tenant)?;
            if doc != TenantLimits::default() {
                limited.push((*tenant, doc));
            }
        }
        if limited.is_empty() {
            return Ok(());
        }
        let (window, resets_at_ns) = month_window(now_ns);
        let mut inner = self.lock()?;
        let mut refusal = None;
        for (tenant, doc) in &limited {
            let month = inner
                .tenants
                .get(*tenant)
                .map(|counters| counters.month_in(&window))
                .unwrap_or_default();
            let (spans, bytes) = tally[tenant];
            let refuse = |dimension, limit: Option<u64>, used| QuotaRefusal {
                tenant: (*tenant).to_owned(),
                dimension,
                limit,
                used,
                window: window.clone(),
                resets_at_ns: (dimension != QuotaDimension::Paused).then_some(resets_at_ns),
                retry_after_seconds: match dimension {
                    QuotaDimension::Paused => PAUSE_RETRY_SECONDS,
                    _ => resets_at_ns.saturating_sub(now_ns).div_ceil(1_000_000_000),
                },
            };
            if doc.ingest_paused {
                refusal = Some(refuse(QuotaDimension::Paused, None, month.spans));
            } else if let Some(limit) = doc
                .monthly_spans
                .filter(|limit| month.spans.saturating_add(spans) > *limit)
            {
                refusal = Some(refuse(
                    QuotaDimension::MonthlySpans,
                    Some(limit),
                    month.spans,
                ));
            } else if let Some(limit) = doc
                .monthly_bytes
                .filter(|limit| month.bytes.saturating_add(bytes) > *limit)
            {
                refusal = Some(refuse(
                    QuotaDimension::MonthlyBytes,
                    Some(limit),
                    month.bytes,
                ));
            }
            if refusal.is_some() {
                break;
            }
        }
        let Some(refusal) = refusal else {
            return Ok(());
        };
        for (tenant, (spans, _)) in &tally {
            let counters = inner.tenants.entry((*tenant).to_owned()).or_default();
            counters.lifetime.rejected = counters.lifetime.rejected.saturating_add(*spans);
        }
        inner.version += 1;
        Err(Error::QuotaExceeded(refusal))
    }

    /// Counts the spans a batch actually admitted.
    ///
    /// `admitted` is an order-preserving subsequence of the batch `admitting`
    /// measured — every span, unless a pending erasure dropped some — so the
    /// measurements are re-aligned by walking both in order and matching
    /// primary-key hashes. A span with no measurement (impossible unless the
    /// hashes collide across a dropped span) is charged its stored size.
    pub(crate) fn record(&self, admitted: &[Arc<Span>], admitting: &Admitting, now_ns: u64) {
        let mut measured = admitting.spans.iter();
        let mut per_tenant: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for span in admitted {
            let key = key_hash(&span.tenant, &span.trace_id, &span.span_id);
            let bytes = measured
                .by_ref()
                .find(|entry| entry.key == key)
                .map_or_else(|| crate::caps::json_len(span.as_ref()), |entry| entry.bytes);
            let slot = per_tenant.entry(span.tenant.as_str()).or_default();
            slot.0 += 1;
            slot.1 = slot.1.saturating_add(bytes);
        }
        if per_tenant.is_empty() {
            return;
        }
        let (window, _) = month_window(now_ns);
        // A poisoned lock loses this batch's counts rather than failing an
        // ingest that is already acknowledged — the documented direction of
        // error is an undercount.
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for (tenant, (spans, bytes)) in per_tenant {
            let counters = inner.tenants.entry(tenant.to_owned()).or_default();
            counters.roll(&window);
            counters.lifetime.spans = counters.lifetime.spans.saturating_add(spans);
            counters.lifetime.bytes = counters.lifetime.bytes.saturating_add(bytes);
            counters.lifetime.batches = counters.lifetime.batches.saturating_add(1);
            counters.month.spans = counters.month.spans.saturating_add(spans);
            counters.month.bytes = counters.month.bytes.saturating_add(bytes);
        }
        inner.version += 1;
    }

    /// Writes `usage.json` if anything changed since the last write.
    pub(crate) fn persist(&self) -> Result<()> {
        let _serial = self
            .persist
            .lock()
            .map_err(|_| Error::LockPoisoned("usage persist"))?;
        let (snapshot, version) = {
            let inner = self.lock()?;
            if inner.version == inner.persisted {
                return Ok(());
            }
            (inner.tenants.clone(), inner.version)
        };
        // The file I/O runs with the counter lock released, so ingest keeps
        // counting through the fsync.
        write_atomically(
            &self.path,
            &serde_json::to_vec(&UsageFile {
                version: 1,
                tenants: snapshot,
            })?,
        )?;
        let mut inner = self.lock()?;
        inner.persisted = inner.persisted.max(version);
        Ok(())
    }

    /// The row for every tenant with counters or limits, or for one tenant.
    pub(crate) fn report(
        &self,
        limits: &LimitsBook,
        tenant: Option<&str>,
        now_ns: u64,
    ) -> Result<UsageReport> {
        let (window, _) = month_window(now_ns);
        let all_limits = limits.all()?;
        let counters = self.lock()?.tenants.clone();
        let names: Vec<String> = match tenant {
            Some(tenant) => vec![tenant.to_owned()],
            None => {
                let mut names: Vec<String> = counters.keys().cloned().collect();
                names.extend(all_limits.keys().cloned());
                names.sort();
                names.dedup();
                names
            }
        };
        let tenants = names
            .into_iter()
            .map(|name| {
                let counted = counters.get(&name).cloned().unwrap_or_default();
                let doc = all_limits.get(&name).cloned().unwrap_or_default();
                let month = counted.month_in(&window);
                UsageRow {
                    state: state_of(&doc, &month),
                    tenant: name,
                    lifetime: counted.lifetime,
                    month,
                    limits: doc,
                }
            })
            .collect();
        Ok(UsageReport {
            as_of_ns: now_ns,
            tenants,
        })
    }

    /// The per-tenant counters in Prometheus text.
    pub(crate) fn render_prometheus(&self, into: &mut String) {
        use std::fmt::Write as _;
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        for (name, read) in [
            (
                "traza_tenant_spans_ingested_total",
                (|counters: &Counters| counters.lifetime.spans) as fn(&Counters) -> u64,
            ),
            ("traza_quota_rejected_total", |counters: &Counters| {
                counters.lifetime.rejected
            }),
        ] {
            let _ = writeln!(into, "# TYPE {name} counter");
            for (tenant, counters) in &inner.tenants {
                let _ = writeln!(
                    into,
                    "{name}{{tenant=\"{}\"}} {}",
                    label_value(tenant),
                    read(counters)
                );
            }
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| Error::LockPoisoned("usage counters"))
    }
}

/// Spans and bytes per tenant in one batch.
fn tally<'a>(
    tenants: impl Iterator<Item = &'a str>,
    admitting: &Admitting,
) -> BTreeMap<&'a str, (u64, u64)> {
    let mut tally: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for (tenant, measured) in tenants.zip(&admitting.spans) {
        let slot = tally.entry(tenant).or_default();
        slot.0 += 1;
        slot.1 = slot.1.saturating_add(measured.bytes);
    }
    tally
}

/// Where `month` stands against `limits`.
fn state_of(limits: &TenantLimits, month: &Month) -> QuotaState {
    if limits.ingest_paused {
        return QuotaState::Paused;
    }
    let pairs = [
        (limits.monthly_spans, month.spans),
        (limits.monthly_bytes, month.bytes),
    ];
    let mut state = QuotaState::Ok;
    for (limit, used) in pairs {
        let Some(limit) = limit else {
            continue;
        };
        if used >= limit {
            return QuotaState::Capped;
        }
        // 80%, in integers: used / limit >= 4/5.
        if u128::from(used) * 5 >= u128::from(limit) * 4 {
            state = QuotaState::Warning;
        }
    }
    state
}

/// The UTC calendar month containing `now_ns` as `YYYY-MM`, and the first
/// nanosecond of the month after it.
pub(crate) fn month_window(now_ns: u64) -> (String, u64) {
    let days = (now_ns / 1_000_000_000 / 86_400) as i64;
    let (year, month, _) = crate::mcp::civil_from_days(days);
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next_days = crate::mcp::days_from_civil(next_year, next_month, 1);
    let resets_at_ns = u64::try_from(next_days)
        .unwrap_or(0)
        .saturating_mul(86_400)
        .saturating_mul(1_000_000_000);
    (format!("{year:04}-{month:02}"), resets_at_ns)
}

/// A Prometheus label value, escaped. Tenant names cannot need it today — the
/// charset is `[a-z0-9._-]` — but the exposition format is not the place to
/// find out that changed.
fn label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl Store {
    /// The usage rows: one tenant's when `tenant` is given (zeros if it has
    /// none yet), else every tenant that has counters or limits. O(tenants);
    /// never reads a segment.
    pub fn usage_report(&self, tenant: Option<&str>) -> Result<UsageReport> {
        self.platform
            .usage
            .report(&self.platform.limits, tenant, crate::unix_now_ns())
    }

    /// Writes the usage counters to `usage.json` if they changed since the
    /// last write. The server calls this every maintenance tick, and
    /// [`Store::flush`] calls it too; an embedder that does neither loses the
    /// counts since its last call when the process ends.
    pub fn persist_usage(&self) -> Result<()> {
        self.platform.usage.persist()
    }

    /// Appends `traza_tenant_spans_ingested_total` and
    /// `traza_quota_rejected_total`, one series per tenant, in Prometheus
    /// text format.
    pub fn render_usage_prometheus(&self, into: &mut String) {
        self.platform.usage.render_prometheus(into);
    }

    /// Counts an acknowledged batch against its tenants' usage.
    pub(crate) fn record_usage(&self, admitted: &[Arc<Span>], admitting: &Admitting) {
        self.platform
            .usage
            .record(admitted, admitting, crate::unix_now_ns());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Measured;
    use serde_json::Map;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// 2026-10-15T12:00:00Z.
    const OCT_15: u64 = 1_792_065_600_000_000_000;

    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "traza-usage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("dir");
        path
    }

    fn span(tenant: &str, id: &str) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: id.into(),
            tenant: tenant.into(),
            parent_span_id: None,
            name: "op".into(),
            start_time_ns: 1,
            end_time_ns: 2,
            status: String::new(),
            service: "svc".into(),
            attributes: Map::new(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn admitting(spans: &[Span], bytes: u64) -> Admitting {
        Admitting {
            spans: spans
                .iter()
                .map(|span| Measured {
                    key: key_hash(&span.tenant, &span.trace_id, &span.span_id),
                    bytes,
                })
                .collect(),
        }
    }

    fn arcs(spans: &[Span]) -> Vec<Arc<Span>> {
        spans.iter().cloned().map(Arc::new).collect()
    }

    #[test]
    fn month_windows_and_their_resets_are_utc_calendar_months() {
        assert_eq!(
            month_window(OCT_15),
            ("2026-10".to_owned(), 1_793_491_200_000_000_000)
        );
        assert_eq!(
            crate::mcp::rfc3339(month_window(OCT_15).1),
            "2026-11-01T00:00:00Z"
        );
        // December rolls the year; the last nanosecond of a month is in it.
        let dec = 1_797_724_800_000_000_000; // 2026-12-20T00:00:00Z
        let (window, resets) = month_window(dec);
        assert_eq!(window, "2026-12");
        assert_eq!(crate::mcp::rfc3339(resets), "2027-01-01T00:00:00Z");
        assert_eq!(month_window(resets - 1).0, "2026-12");
        assert_eq!(month_window(resets).0, "2027-01");
    }

    #[test]
    fn admitted_batches_count_per_tenant_and_the_month_rolls_over() {
        let path = dir();
        let book = UsageBook::open(&path).expect("opens");
        let limits = LimitsBook::open(&path).expect("opens");
        let batch = [span("acme", "a"), span("acme", "b"), span("", "c")];
        book.record(&arcs(&batch), &admitting(&batch, 100), OCT_15);
        book.record(&arcs(&batch[..1]), &admitting(&batch[..1], 100), OCT_15);
        let report = book.report(&limits, None, OCT_15).expect("reports");
        assert_eq!(
            report
                .tenants
                .iter()
                .map(|row| row.tenant.as_str())
                .collect::<Vec<_>>(),
            vec!["", "acme"]
        );
        let acme = &report.tenants[1];
        assert_eq!(
            acme.lifetime,
            Lifetime {
                spans: 3,
                bytes: 300,
                batches: 2,
                rejected: 0
            }
        );
        assert_eq!((acme.month.spans, acme.month.bytes), (3, 300));
        assert_eq!(acme.state, QuotaState::Ok);

        // A month later the month reads zero and the lifetime does not.
        let november = month_window(OCT_15).1 + 1;
        let row = book
            .report(&limits, Some("acme"), november)
            .expect("reports")
            .tenants
            .remove(0);
        assert_eq!((row.month.window.as_str(), row.month.spans), ("2026-11", 0));
        assert_eq!(row.lifetime.spans, 3);
        book.record(&arcs(&batch[..1]), &admitting(&batch[..1], 7), november);
        let row = book
            .report(&limits, Some("acme"), november)
            .expect("reports")
            .tenants
            .remove(0);
        assert_eq!((row.month.spans, row.month.bytes), (1, 7));
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_partly_suppressed_batch_is_charged_only_for_what_was_admitted() {
        let path = dir();
        let book = UsageBook::open(&path).expect("opens");
        let limits = LimitsBook::open(&path).expect("opens");
        let batch = [
            span("acme", "a"),
            span("acme", "dropped"),
            span("acme", "c"),
        ];
        let mut measured = admitting(&batch, 0);
        for (index, entry) in measured.spans.iter_mut().enumerate() {
            entry.bytes = [10, 1_000, 30][index];
        }
        let admitted = arcs(&[batch[0].clone(), batch[2].clone()]);
        book.record(&admitted, &measured, OCT_15);
        let row = book
            .report(&limits, Some("acme"), OCT_15)
            .expect("ok")
            .tenants[0]
            .clone();
        assert_eq!((row.lifetime.spans, row.lifetime.bytes), (2, 40));
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn quotas_refuse_whole_batches_and_count_the_refusal() {
        let path = dir();
        let book = UsageBook::open(&path).expect("opens");
        let limits = LimitsBook::open(&path).expect("opens");
        limits
            .set(
                "acme",
                TenantLimits {
                    monthly_spans: Some(3),
                    monthly_bytes: Some(1_000),
                    ..TenantLimits::default()
                },
            )
            .expect("sets");
        let two = [span("acme", "a"), span("acme", "b")];
        book.check(&limits, &two, &admitting(&two, 10), OCT_15)
            .expect("2 of 3");
        book.record(&arcs(&two), &admitting(&two, 10), OCT_15);
        let refused = book
            .check(&limits, &two, &admitting(&two, 10), OCT_15)
            .expect_err("2 more would be 4 of 3");
        let Error::QuotaExceeded(refusal) = refused else {
            panic!("{refused:?}");
        };
        assert_eq!(refusal.dimension, QuotaDimension::MonthlySpans);
        assert_eq!((refusal.limit, refusal.used), (Some(3), 2));
        assert_eq!(refusal.window, "2026-10");
        assert_eq!(
            refusal.retry_after_seconds, 1_425_600,
            "16.5 days, rounded up"
        );
        assert_eq!(
            refusal.to_json(),
            json!({"error": "quota exceeded: monthly_spans for tenant acme",
                   "quota": {"tenant": "acme", "dimension": "monthly_spans", "limit": 3,
                             "used": 2, "window": "2026-10",
                             "resets_at": "2026-11-01T00:00:00Z"}})
        );
        let one = [span("acme", "c")];
        book.check(&limits, &one, &admitting(&one, 10), OCT_15)
            .expect("exactly at the limit is allowed");
        let heavy = [span("acme", "d")];
        // The span count fits (2 + 1 of 3); the bytes do not (20 + 981).
        let Err(Error::QuotaExceeded(bytes)) =
            book.check(&limits, &heavy, &admitting(&heavy, 981), OCT_15)
        else {
            panic!("bytes over");
        };
        assert_eq!(bytes.dimension, QuotaDimension::MonthlyBytes);
        assert_eq!(bytes.used, 20);

        let row = book
            .report(&limits, Some("acme"), OCT_15)
            .expect("ok")
            .tenants[0]
            .clone();
        assert_eq!(row.lifetime.rejected, 3, "2 then 1 refused spans");
        assert_eq!(row.state, QuotaState::Ok, "2 of 3 is under 80%");
        book.record(&arcs(&one), &admitting(&one, 10), OCT_15);
        let row = book
            .report(&limits, Some("acme"), OCT_15)
            .expect("ok")
            .tenants[0]
            .clone();
        assert_eq!(row.state, QuotaState::Capped, "3 of 3");
        // A tenant without limits in a mixed batch is refused with it, and
        // its spans are counted as ITS rejections.
        let mixed = [span("acme", "e"), span("acme", "f"), span("other", "g")];
        assert!(book
            .check(&limits, &mixed, &admitting(&mixed, 1), OCT_15)
            .is_err());
        let other = book
            .report(&limits, Some("other"), OCT_15)
            .expect("ok")
            .tenants[0]
            .clone();
        assert_eq!(other.lifetime.rejected, 1);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_pause_refuses_everything_and_reports_paused() {
        let path = dir();
        let book = UsageBook::open(&path).expect("opens");
        let limits = LimitsBook::open(&path).expect("opens");
        limits
            .set(
                "",
                TenantLimits {
                    ingest_paused: true,
                    ..TenantLimits::default()
                },
            )
            .expect("sets");
        let one = [span("", "a")];
        let Err(Error::QuotaExceeded(refusal)) =
            book.check(&limits, &one, &admitting(&one, 1), OCT_15)
        else {
            panic!("paused");
        };
        assert_eq!(refusal.dimension, QuotaDimension::Paused);
        assert_eq!(refusal.retry_after_seconds, 60);
        assert_eq!(refusal.resets_at_ns, None);
        assert_eq!(
            refusal.to_string(),
            "quota exceeded: paused for the default tenant"
        );
        let row = book.report(&limits, Some(""), OCT_15).expect("ok").tenants[0].clone();
        assert_eq!(row.state, QuotaState::Paused);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn counters_persist_only_when_changed_and_survive_a_reopen() {
        let path = dir();
        {
            let book = UsageBook::open(&path).expect("opens");
            book.persist().expect("nothing to write");
            assert!(
                !path.join(FILE_NAME).exists(),
                "an idle book writes nothing"
            );
            let batch = [span("acme", "a")];
            book.record(&arcs(&batch), &admitting(&batch, 5), OCT_15);
            book.persist().expect("writes");
            let written = fs::metadata(path.join(FILE_NAME))
                .expect("file")
                .modified()
                .ok();
            book.persist().expect("unchanged");
            assert_eq!(
                fs::metadata(path.join(FILE_NAME))
                    .expect("file")
                    .modified()
                    .ok(),
                written
            );
        }
        let book = UsageBook::open(&path).expect("reopens");
        let limits = LimitsBook::open(&path).expect("opens");
        let row = book
            .report(&limits, Some("acme"), OCT_15)
            .expect("ok")
            .tenants[0]
            .clone();
        assert_eq!((row.lifetime.spans, row.lifetime.bytes), (1, 5));

        fs::write(path.join(FILE_NAME), b"{").expect("damage");
        assert!(UsageBook::open(&path).is_err());
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn states_follow_the_eighty_percent_rule() {
        let month = |spans| Month {
            window: "2026-10".into(),
            spans,
            bytes: 0,
        };
        let limits = TenantLimits {
            monthly_spans: Some(10),
            ..TenantLimits::default()
        };
        assert_eq!(state_of(&limits, &month(7)), QuotaState::Ok);
        assert_eq!(state_of(&limits, &month(8)), QuotaState::Warning);
        assert_eq!(state_of(&limits, &month(10)), QuotaState::Capped);
        assert_eq!(
            state_of(&TenantLimits::default(), &month(u64::MAX)),
            QuotaState::Ok
        );
    }

    #[test]
    fn prometheus_series_carry_the_tenant_label() {
        let path = dir();
        let book = UsageBook::open(&path).expect("opens");
        let batch = [span("acme", "a")];
        book.record(&arcs(&batch), &admitting(&batch, 5), OCT_15);
        let mut text = String::new();
        book.render_prometheus(&mut text);
        assert!(text.contains("# TYPE traza_tenant_spans_ingested_total counter"));
        assert!(text.contains("traza_tenant_spans_ingested_total{tenant=\"acme\"} 1"));
        assert!(text.contains("traza_quota_rejected_total{tenant=\"acme\"} 0"));
        assert_eq!(label_value("a\"b\\c"), "a\\\"b\\\\c");
        let _ = fs::remove_dir_all(&path);
    }
}
