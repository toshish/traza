//! The eval entity model: datasets, versions, examples, experiments, runs —
//! identity and addressing only, no workflow.
//!
//! This exists because today's telemetry cannot REPRESENT the eval loop: a
//! failing production trace has nowhere to be promoted to, an experiment has
//! no identity to hang scores on, and an `Annotation` addressed only to
//! `(trace_id, span_id)` cannot express a score against an
//! `(experiment, example, span)` tuple. The entities here are the addressing
//! that closes that — deliberately WITHOUT a runner, a scorer library, or a
//! UI, because the loop being representable is the freeze-critical part and
//! the workflow is not.
//!
//! Everything lives in one append-only JSONL log (`evals.jsonl` at the store
//! root), a manifested recovery domain exactly like `annotations.jsonl` and
//! `tombstones.jsonl`: fsync per mutation, torn-tail healing at open, prefix
//! digests in every generation manifest, copied — never hard-linked — into
//! pins. It is rewritten only inside the erasure barrier, when a tenant
//! subject removes everything a tenant owns.
//!
//! **Memory stance**: the whole log is resident, example bodies included.
//! Datasets are curated artifacts — hundreds of examples, not millions of
//! spans — and a promoted example's LARGE values arrive as `$payload`
//! references, not inline bytes (the source span was offloaded at ingest, and
//! the reference is what the promotion copies). The same stance the
//! annotation log takes, for the same reason.
//!
//! **Content addressing**: example bodies and version manifests are addressed
//! by SHA-256 over [`canonical_json`], Traza's own canonical form — key order
//! and number formatting are pinned by test the way segment bytes are,
//! because these digests are persisted identity and must never depend on a
//! dependency's map-ordering feature flag.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::erasure::Mask;
use crate::{Error, Result};

/// File name of the eval log, at the store root beside `annotations.jsonl`.
pub(crate) const LOG_NAME: &str = "evals.jsonl";

/// Record schema version, per record.
const SCHEMA: u32 = 1;

// ------------------------------------------------------------ canonical JSON

/// Serializes a JSON value canonically: object keys sorted bytewise, no
/// whitespace, strings in serde_json's escaping, numbers in serde_json's
/// formatting (exact integers; shortest round-trip floats).
///
/// The sort is performed HERE, not inherited from `serde_json::Map`'s
/// iteration order: that order is a Cargo feature (`preserve_order`) any
/// future dependency could flip by feature unification, silently changing
/// every digest computed after the rebuild while the persisted ones stay.
/// Identity must not be one `cargo update` away from diverging. Pinned by
/// byte-level fixtures in this module's tests.
pub(crate) fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => {
            out.push_str(&serde_json::to_string(text).expect("strings always serialize"));
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("strings always serialize"));
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
    }
}

// ------------------------------------------------------------------ records

/// Where an example came from: the source trace/span it was promoted from.
/// Optional throughout — an imported dataset has no source span — but when
/// present it is what lets an erasure receipt point at the copy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Tenant of the source span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// Source trace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trace_id: String,
    /// Source span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub span_id: String,
}

/// One example's content — the copy a dataset carries so that deleting the
/// source trace cannot corrupt the dataset version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExampleBody {
    /// The task input. May contain `$payload` references copied from the
    /// source span; those count as live references for the TTL sweep and
    /// reference-aware erasure, which is what makes the copy real for
    /// offloaded content.
    pub input: Value,
    /// The expected output, when the dataset has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    /// Split label (`train`, `test`, …); free-form.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub split: String,
    /// Where this example came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl ExampleBody {
    /// The body's content address: SHA-256 over its canonical JSON.
    pub(crate) fn digest(&self) -> String {
        let value = serde_json::to_value(self).expect("body serializes");
        crate::payload::sha256_hex(canonical_json(&value).as_bytes())
    }
}

/// A dataset: a stable id, a name, a tenant. Versions carry the content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dataset {
    /// Record schema version.
    pub schema: u32,
    /// Store-wide id; monotonic, never reused (see the counters record).
    pub dataset_id: u64,
    /// Owning tenant; empty is the default tenant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// Human name; not an identity.
    pub name: String,
    /// Wall-clock creation time, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// A content-addressed example body, shared by every version that lists its
/// digest. Carries no tenant: identical content promoted by two tenants is
/// one record, and reachability — whose versions list it — is what scoping
/// walks follow.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExampleRecord {
    /// Record schema version.
    pub schema: u32,
    /// SHA-256 (hex) of the canonical body JSON.
    pub digest: String,
    /// The content.
    pub body: ExampleBody,
}

/// An immutable, content-addressed dataset version: the manifest of example
/// ids and digests, its parent for lineage, and the provenance of the
/// promotion that produced it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatasetVersion {
    /// Record schema version.
    pub schema: u32,
    /// Owning tenant, denormalized from the dataset for scoped walks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The dataset this version belongs to.
    pub dataset_id: u64,
    /// SHA-256 (hex) of the canonical manifest — `{dataset_id, parent,
    /// examples}` — so identical content IS the identical version and a
    /// re-POST is idempotent by construction.
    pub version_id: String,
    /// The version this one was derived from, for lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// What produced this version — a query, an import description. Free
    /// JSON; first write wins on an idempotent re-POST.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
    /// `(example_id, digest)` pairs, sorted by example id. Example ids are
    /// STABLE ACROSS VERSIONS by construction: the id is client-chosen and
    /// the digest is content, so a version that re-lists an id with new
    /// content is new lineage for the same logical example.
    pub examples: Vec<(String, String)>,
    /// Wall-clock creation time, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// An experiment: one dataset version, configuration metadata, and — via
/// [`Run`] records and score annotations — the set of task runs. Stable id,
/// monotonic, never reused.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Experiment {
    /// Record schema version.
    pub schema: u32,
    /// Store-wide id; monotonic, never reused.
    pub experiment_id: u64,
    /// Owning tenant, inherited from the dataset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The dataset under evaluation.
    pub dataset_id: u64,
    /// The exact version the experiment ran against.
    pub dataset_version: String,
    /// Human name; not an identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Configuration metadata (model, prompt hash, temperature, …). Free
    /// JSON, recorded verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// Wall-clock creation time, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// One task run: the experiment→trace link the roadmap names as part of the
/// experiment entity. Appended by the external harness; task execution stays
/// outside Traza. Append-only, duplicates legal (a retried example is two
/// runs).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    /// Record schema version.
    pub schema: u32,
    /// Owning tenant, inherited from the experiment.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The experiment this run belongs to.
    pub experiment_id: u64,
    /// The example the run executed.
    pub example_id: String,
    /// The trace the run produced.
    pub trace_id: String,
    /// The run's span within the trace, when the harness knows it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub span_id: String,
    /// Wall-clock record time, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// A dataset version's tombstone: LOGICAL deletion with defined effects —
/// the version leaves listings and returns 410, dependent experiments keep
/// working but report it, new experiments against it are refused, scores are
/// untouched, and example bodies stay in the log (their payload references
/// keep counting as live) until a future eval compaction reclaims
/// version-unreachable bodies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionTombstone {
    /// Record schema version.
    pub schema: u32,
    /// Owning tenant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The dataset whose version is tombstoned.
    pub dataset_id: u64,
    /// The tombstoned version.
    pub version_id: String,
    /// Wall-clock request time, nanoseconds since the Unix epoch.
    pub requested_unix_ns: u64,
    /// Free-form reason, recorded verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// Id floors surviving a tenant-erasure rewrite of the log. Ids are
/// "monotonic, never reused"; a rewrite that dropped the highest-id records
/// would otherwise let the next allocation reissue them, silently aliasing
/// erased entities in every external reference (CI configs, receipts,
/// operator notes).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Counters {
    /// Record schema version.
    schema: u32,
    /// Next dataset id at or above this.
    dataset_next: u64,
    /// Next experiment id at or above this.
    experiment_next: u64,
}

/// One line of the eval log.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum LogRecord {
    Dataset(Dataset),
    Example(ExampleRecord),
    DatasetVersion(DatasetVersion),
    Experiment(Experiment),
    Run(Run),
    DatasetVersionTombstone(VersionTombstone),
    Counters(Counters),
}

// ------------------------------------------------------------------- views

/// A dataset with its versions summarized — the list/detail read shape.
#[derive(Clone, Debug, Serialize)]
pub struct DatasetView {
    /// The dataset record's fields.
    #[serde(flatten)]
    pub dataset: Dataset,
    /// Version summaries, oldest first.
    pub versions: Vec<VersionSummary>,
}

/// One version row in a dataset view.
#[derive(Clone, Debug, Serialize)]
pub struct VersionSummary {
    /// The version's content address.
    pub version_id: String,
    /// Its parent, for lineage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// How many examples the manifest lists.
    pub examples: usize,
    /// Creation time.
    pub created_unix_ns: u64,
    /// Whether a tombstone hides it.
    pub tombstoned: bool,
}

/// A version with its example bodies — the version-detail read shape.
#[derive(Clone, Debug, Serialize)]
pub struct VersionView {
    /// The version record.
    #[serde(flatten)]
    pub version: DatasetVersion,
    /// The manifest's bodies, in manifest order.
    pub bodies: Vec<ExampleWithBody>,
}

/// One example id with its content.
#[derive(Clone, Debug, Serialize)]
pub struct ExampleWithBody {
    /// Stable example id.
    pub example_id: String,
    /// Content address of the body.
    pub digest: String,
    /// The body itself.
    pub body: ExampleBody,
}

/// An experiment with derived state — the experiment read shape.
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentView {
    /// The experiment record.
    #[serde(flatten)]
    pub experiment: Experiment,
    /// Whether the version it ran against has been tombstoned since. The
    /// experiment keeps working — its scores and runs are its own — but a
    /// reader deserves to know the dataset content is no longer served.
    pub dataset_version_deleted: bool,
    /// Recorded task runs.
    pub run_count: usize,
}

/// A new example as supplied to a version POST.
#[derive(Clone, Debug, Deserialize)]
pub struct NewExample {
    /// Stable, client-chosen example id — the identity that persists across
    /// versions.
    pub example_id: String,
    /// See [`ExampleBody::input`].
    pub input: Value,
    /// See [`ExampleBody::expected`].
    #[serde(default)]
    pub expected: Option<Value>,
    /// See [`ExampleBody::split`].
    #[serde(default)]
    pub split: String,
    /// See [`ExampleBody::provenance`].
    #[serde(default)]
    pub provenance: Option<Provenance>,
}

/// What a version POST produced.
#[derive(Clone, Debug, Serialize)]
pub struct VersionOutcome {
    /// The version's content address.
    pub version_id: String,
    /// Examples in its manifest.
    pub examples: usize,
    /// False when the identical version already existed (idempotent
    /// re-POST).
    pub created: bool,
}

// --------------------------------------------------------------------- log

/// The append-only eval log plus its replayed state. The single mutex spans
/// every validate+append pair — check-then-append races (concurrent
/// identical version POSTs, an experiment racing its version's tombstone, a
/// score validating against a mutating manifest) are excluded by
/// construction, not by care. Leaf-level, like the annotation log's: never
/// held while taking an engine lock.
#[derive(Debug)]
pub(crate) struct EvalLog {
    path: PathBuf,
    inner: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    datasets: BTreeMap<u64, Dataset>,
    versions: HashMap<String, DatasetVersion>,
    /// Version ids per dataset, in append order.
    versions_by_dataset: BTreeMap<u64, Vec<String>>,
    examples: HashMap<String, ExampleRecord>,
    experiments: BTreeMap<u64, Experiment>,
    runs: Vec<Run>,
    tombstones: HashMap<String, VersionTombstone>,
    /// Id floors from counters records; allocation never dips below these.
    dataset_floor: u64,
    experiment_floor: u64,
    /// Every `$payload` reference any example body carries — the eval side
    /// of the live set protecting payload files.
    payload_refs: HashSet<String>,
}

impl State {
    fn apply(&mut self, record: LogRecord) {
        match record {
            LogRecord::Dataset(dataset) => {
                self.datasets.insert(dataset.dataset_id, dataset);
            }
            LogRecord::Example(example) => {
                collect_refs(
                    &serde_json::to_value(&example.body).expect("body serializes"),
                    &mut self.payload_refs,
                );
                self.examples.insert(example.digest.clone(), example);
            }
            LogRecord::DatasetVersion(version) => {
                // Idempotent by content address: a duplicate line (crash
                // between append and ack, then a retry) re-inserts an equal
                // record.
                self.versions_by_dataset
                    .entry(version.dataset_id)
                    .or_default()
                    .retain(|existing| *existing != version.version_id);
                self.versions_by_dataset
                    .entry(version.dataset_id)
                    .or_default()
                    .push(version.version_id.clone());
                self.versions.insert(version.version_id.clone(), version);
            }
            LogRecord::Experiment(experiment) => {
                self.experiments
                    .insert(experiment.experiment_id, experiment);
            }
            LogRecord::Run(run) => self.runs.push(run),
            LogRecord::DatasetVersionTombstone(tombstone) => {
                self.tombstones
                    .insert(tombstone.version_id.clone(), tombstone);
            }
            LogRecord::Counters(counters) => {
                self.dataset_floor = self.dataset_floor.max(counters.dataset_next);
                self.experiment_floor = self.experiment_floor.max(counters.experiment_next);
            }
        }
    }

    fn next_dataset_id(&self) -> u64 {
        self.datasets
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
            .max(self.dataset_floor.saturating_add(1).max(1))
    }

    fn next_experiment_id(&self) -> u64 {
        self.experiments
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
            .max(self.experiment_floor.saturating_add(1).max(1))
    }

    /// Whether `tenant` may read/name this dataset under `scope`: `None`
    /// (operator) sees everything, a bound scope only its own. NOT FOUND is
    /// the answer for a foreign entity — existence is a fact about another
    /// tenant.
    fn dataset_in_scope(&self, scope: Option<&str>, dataset_id: u64) -> Option<&Dataset> {
        let dataset = self.datasets.get(&dataset_id)?;
        match scope {
            Some(tenant) if dataset.tenant != tenant => None,
            _ => Some(dataset),
        }
    }

    fn experiment_in_scope(&self, scope: Option<&str>, experiment_id: u64) -> Option<&Experiment> {
        let experiment = self.experiments.get(&experiment_id)?;
        match scope {
            Some(tenant) if experiment.tenant != tenant => None,
            _ => Some(experiment),
        }
    }
}

/// Collects `$payload` references from anywhere inside a JSON value —
/// example bodies nest them wherever the promoted span had them. Redaction
/// markers (`"erased": true`) are not references, same rule as span
/// collection.
fn collect_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if map.get("erased").and_then(Value::as_bool) == Some(true) {
                return;
            }
            if let Some(reference) = map.get(crate::payload::PAYLOAD_KEY).and_then(Value::as_str) {
                refs.insert(reference.to_owned());
            }
            for nested in map.values() {
                collect_refs(nested, refs);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, refs);
            }
        }
        _ => {}
    }
}

impl EvalLog {
    /// Opens the log, replaying existing records. Torn-tail healing and the
    /// missing-newline repair are the annotation log's, verbatim in spirit:
    /// a torn final line is an append the crash interrupted; a malformed
    /// interior line is corruption and fails the open.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut state = State::default();
        if path.exists() {
            let contents = fs::read(&path)?;
            let terminated = contents.ends_with(b"\n");
            let mut lines = contents.split_inclusive(|byte| *byte == b'\n').peekable();
            let mut valid_len = 0_u64;
            let mut truncated_torn_tail = false;
            while let Some(line) = lines.next() {
                if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                    valid_len = valid_len.saturating_add(line.len() as u64);
                    continue;
                }
                match serde_json::from_slice::<LogRecord>(line) {
                    Ok(record) => {
                        valid_len = valid_len.saturating_add(line.len() as u64);
                        state.apply(record);
                    }
                    Err(_) if lines.peek().is_none() && !terminated => {
                        let file = OpenOptions::new().write(true).open(&path)?;
                        file.set_len(valid_len)?;
                        file.sync_all()?;
                        truncated_torn_tail = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if !contents.is_empty() && !terminated && !truncated_torn_tail {
                let mut file = OpenOptions::new().append(true).open(&path)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
        }
        Ok(Self {
            path,
            inner: Mutex::new(state),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.inner.lock().map_err(|_| Error::LockPoisoned("evals"))
    }

    /// Appends `records` as consecutive lines with ONE fsync. Multi-record
    /// mutations (bodies before their version) rely on the order: a torn
    /// tail can strand orphan bodies — documented leak, reclaimed by a
    /// future compaction — but can never produce a version whose bodies are
    /// missing.
    fn append_lines(&self, records: &[LogRecord]) -> Result<()> {
        let mut buffer = String::new();
        for record in records {
            buffer.push_str(&serde_json::to_string(record)?);
            buffer.push('\n');
        }
        let created = !self.path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(buffer.as_bytes())?;
        file.sync_all()?;
        if created {
            if let Some(directory) = self.path.parent() {
                crate::sync_directory(directory)?;
            }
        }
        Ok(())
    }

    /// Refuses a mutation for a tenant a pending erasure covers. The caller
    /// holds the erasure gate's read half, so this check and the append are
    /// wholly before `begin` or wholly after — never astride it.
    fn barrier(mask: Option<&Mask>, tenant: &str) -> Result<()> {
        if mask.is_some_and(|mask| mask.covers_tenant(tenant)) {
            return Err(Error::Conflict(format!(
                "tenant {tenant} has an erasure pending; retry after it settles"
            )));
        }
        Ok(())
    }

    /// Creates a dataset and returns its id.
    pub(crate) fn create_dataset(
        &self,
        mask: Option<&Mask>,
        tenant: &str,
        name: &str,
        created_unix_ns: u64,
    ) -> Result<u64> {
        if name.is_empty() {
            return Err(Error::InvalidSpan("dataset name is empty"));
        }
        if !tenant.is_empty() && !crate::valid_tenant(tenant) {
            return Err(Error::InvalidSpan(
                "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
            ));
        }
        Self::barrier(mask, tenant)?;
        let mut state = self.lock()?;
        let dataset = Dataset {
            schema: SCHEMA,
            dataset_id: state.next_dataset_id(),
            tenant: tenant.to_owned(),
            name: name.to_owned(),
            created_unix_ns,
        };
        self.append_lines(&[LogRecord::Dataset(dataset.clone())])?;
        let id = dataset.dataset_id;
        state.apply(LogRecord::Dataset(dataset));
        Ok(id)
    }

    /// Creates (or idempotently re-finds) a dataset version.
    ///
    /// `verify_ref` is the payload interlock: for every `$payload` reference
    /// a body carries it must TOUCH the reference in the recent-payloads
    /// registry and then answer whether the bytes exist — the same
    /// touch-before-check discipline `store_payload` uses, which is what
    /// closes the race against a concurrent TTL sweep. A reference that is
    /// pending erasure or absent refuses the POST: an example born dangling
    /// would make "examples carry their own copies" a lie at birth.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_version(
        &self,
        mask: Option<&Mask>,
        scope: Option<&str>,
        dataset_id: u64,
        parent: Option<String>,
        provenance: Option<Value>,
        examples: Vec<NewExample>,
        created_unix_ns: u64,
        verify_ref: &dyn Fn(&str) -> Result<bool>,
    ) -> Result<VersionOutcome> {
        if examples.is_empty() {
            return Err(Error::InvalidSpan("a version needs at least one example"));
        }
        let state = self.lock()?;
        let Some(dataset) = state.dataset_in_scope(scope, dataset_id) else {
            return Err(Error::Invalid(format!("no dataset {dataset_id}")));
        };
        let tenant = dataset.tenant.clone();
        Self::barrier(mask, &tenant)?;
        if let Some(parent) = &parent {
            let known = state
                .versions
                .get(parent)
                .is_some_and(|version| version.dataset_id == dataset_id);
            if !known {
                return Err(Error::Invalid(format!(
                    "parent version {parent} is not a version of dataset {dataset_id}"
                )));
            }
            if state.tombstones.contains_key(parent) {
                return Err(Error::Conflict(format!(
                    "parent version {parent} is tombstoned"
                )));
            }
        }

        // Build the manifest: ids unique and non-empty, sorted for the
        // content address.
        let mut manifest: Vec<(String, String, ExampleBody)> = Vec::with_capacity(examples.len());
        let mut seen: HashSet<String> = HashSet::new();
        for example in examples {
            if example.example_id.is_empty() {
                return Err(Error::InvalidSpan("example_id is empty"));
            }
            if !seen.insert(example.example_id.clone()) {
                return Err(Error::Invalid(format!(
                    "example {} appears twice in the manifest",
                    example.example_id
                )));
            }
            let body = ExampleBody {
                input: example.input,
                expected: example.expected,
                split: example.split,
                provenance: example.provenance,
            };
            let digest = body.digest();
            manifest.push((example.example_id, digest, body));
        }
        manifest.sort_by(|left, right| left.0.cmp(&right.0));

        // The payload interlock, before anything is appended.
        let mut refs: HashSet<String> = HashSet::new();
        for (_, _, body) in &manifest {
            collect_refs(
                &serde_json::to_value(body).expect("body serializes"),
                &mut refs,
            );
        }
        for reference in &refs {
            if mask.is_some_and(|mask| mask.covers_payload_file(reference)) {
                return Err(Error::Conflict(format!(
                    "payload {reference} is pending erasure; the promotion conflicts with it"
                )));
            }
            if !verify_ref(reference)? {
                return Err(Error::Conflict(format!(
                    "payload {reference} is not in the store; the example would be born dangling"
                )));
            }
        }

        let manifest_pairs: Vec<(String, String)> = manifest
            .iter()
            .map(|(id, digest, _)| (id.clone(), digest.clone()))
            .collect();
        let manifest_value = serde_json::json!({
            "dataset_id": dataset_id,
            "parent": parent,
            "examples": manifest_pairs,
        });
        let version_id = crate::payload::sha256_hex(canonical_json(&manifest_value).as_bytes());
        if state.versions.contains_key(&version_id) {
            // Identical content, identical identity: the earlier record —
            // its provenance included — stands.
            return Ok(VersionOutcome {
                version_id,
                examples: manifest_pairs.len(),
                created: false,
            });
        }

        let mut records: Vec<LogRecord> = Vec::new();
        for (_, digest, body) in &manifest {
            if !state.examples.contains_key(digest)
                && !records.iter().any(|record| {
                    matches!(record, LogRecord::Example(example) if example.digest == *digest)
                })
            {
                records.push(LogRecord::Example(ExampleRecord {
                    schema: SCHEMA,
                    digest: digest.clone(),
                    body: body.clone(),
                }));
            }
        }
        records.push(LogRecord::DatasetVersion(DatasetVersion {
            schema: SCHEMA,
            tenant,
            dataset_id,
            version_id: version_id.clone(),
            parent,
            provenance,
            examples: manifest_pairs.clone(),
            created_unix_ns,
        }));
        drop(state);
        // Re-acquire for the mutation: the append and the in-memory apply
        // happen under one hold; the validation above held the same mutex,
        // and the only writer between the two holds is... nobody — `self`
        // methods all lock this mutex, so a re-check is about discipline.
        let mut state = self.lock()?;
        if state.versions.contains_key(&version_id) {
            return Ok(VersionOutcome {
                version_id,
                examples: manifest_pairs.len(),
                created: false,
            });
        }
        self.append_lines(&records)?;
        for record in records {
            state.apply(record);
        }
        Ok(VersionOutcome {
            version_id,
            examples: manifest_pairs.len(),
            created: true,
        })
    }

    /// Creates an experiment against a dataset version.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_experiment(
        &self,
        mask: Option<&Mask>,
        scope: Option<&str>,
        dataset_id: u64,
        dataset_version: &str,
        name: &str,
        config: Option<Value>,
        created_unix_ns: u64,
    ) -> Result<u64> {
        let mut state = self.lock()?;
        let Some(dataset) = state.dataset_in_scope(scope, dataset_id) else {
            return Err(Error::Invalid(format!("no dataset {dataset_id}")));
        };
        let tenant = dataset.tenant.clone();
        Self::barrier(mask, &tenant)?;
        let known = state
            .versions
            .get(dataset_version)
            .is_some_and(|version| version.dataset_id == dataset_id);
        if !known {
            return Err(Error::Invalid(format!(
                "version {dataset_version} is not a version of dataset {dataset_id}"
            )));
        }
        if state.tombstones.contains_key(dataset_version) {
            return Err(Error::Conflict(format!(
                "version {dataset_version} is tombstoned; new experiments cannot run against it"
            )));
        }
        let experiment = Experiment {
            schema: SCHEMA,
            experiment_id: state.next_experiment_id(),
            tenant,
            dataset_id,
            dataset_version: dataset_version.to_owned(),
            name: name.to_owned(),
            config,
            created_unix_ns,
        };
        self.append_lines(&[LogRecord::Experiment(experiment.clone())])?;
        let id = experiment.experiment_id;
        state.apply(LogRecord::Experiment(experiment));
        Ok(id)
    }

    /// Records one task run against an experiment's example.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_run(
        &self,
        mask: Option<&Mask>,
        scope: Option<&str>,
        experiment_id: u64,
        example_id: &str,
        trace_id: &str,
        span_id: &str,
        created_unix_ns: u64,
    ) -> Result<()> {
        if trace_id.is_empty() {
            return Err(Error::InvalidSpan("run trace_id is empty"));
        }
        let mut state = self.lock()?;
        let Some(experiment) = state.experiment_in_scope(scope, experiment_id) else {
            return Err(Error::Invalid(format!("no experiment {experiment_id}")));
        };
        let tenant = experiment.tenant.clone();
        let version_id = experiment.dataset_version.clone();
        Self::barrier(mask, &tenant)?;
        let in_manifest = state
            .versions
            .get(&version_id)
            .is_some_and(|version| version.examples.iter().any(|(id, _)| id == example_id));
        if !in_manifest {
            return Err(Error::Invalid(format!(
                "example {example_id} is not in experiment {experiment_id}'s dataset version"
            )));
        }
        let run = Run {
            schema: SCHEMA,
            tenant,
            experiment_id,
            example_id: example_id.to_owned(),
            trace_id: trace_id.to_owned(),
            span_id: span_id.to_owned(),
            created_unix_ns,
        };
        self.append_lines(&[LogRecord::Run(run.clone())])?;
        state.apply(LogRecord::Run(run));
        Ok(())
    }

    /// Tombstones a dataset version. Idempotent: returns `false` when it
    /// already was.
    pub(crate) fn tombstone_version(
        &self,
        mask: Option<&Mask>,
        scope: Option<&str>,
        dataset_id: u64,
        version_id: &str,
        reason: &str,
        requested_unix_ns: u64,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let Some(dataset) = state.dataset_in_scope(scope, dataset_id) else {
            return Err(Error::Invalid(format!("no dataset {dataset_id}")));
        };
        let tenant = dataset.tenant.clone();
        Self::barrier(mask, &tenant)?;
        let known = state
            .versions
            .get(version_id)
            .is_some_and(|version| version.dataset_id == dataset_id);
        if !known {
            return Err(Error::Invalid(format!(
                "version {version_id} is not a version of dataset {dataset_id}"
            )));
        }
        if state.tombstones.contains_key(version_id) {
            return Ok(false);
        }
        let tombstone = VersionTombstone {
            schema: SCHEMA,
            tenant,
            dataset_id,
            version_id: version_id.to_owned(),
            requested_unix_ns,
            reason: reason.to_owned(),
        };
        self.append_lines(&[LogRecord::DatasetVersionTombstone(tombstone.clone())])?;
        state.apply(LogRecord::DatasetVersionTombstone(tombstone));
        Ok(true)
    }

    /// Validates a score's address under the log's own mutex: the experiment
    /// exists, belongs to the score's tenant, and lists the example.
    pub(crate) fn validate_score(
        &self,
        tenant: &str,
        experiment_id: u64,
        example_id: &str,
    ) -> Result<()> {
        let state = self.lock()?;
        let Some(experiment) = state.experiments.get(&experiment_id) else {
            return Err(Error::Invalid(format!("no experiment {experiment_id}")));
        };
        if experiment.tenant != tenant {
            return Err(Error::Invalid(format!(
                "a score's tenant must be its experiment's; experiment {experiment_id} disagrees"
            )));
        }
        let in_manifest = state
            .versions
            .get(&experiment.dataset_version)
            .is_some_and(|version| version.examples.iter().any(|(id, _)| id == example_id));
        if !in_manifest {
            return Err(Error::Invalid(format!(
                "example {example_id} is not in experiment {experiment_id}'s dataset version"
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------- reads

    /// Datasets visible under `scope`, versions summarized.
    pub(crate) fn datasets(&self, scope: Option<&str>) -> Result<Vec<DatasetView>> {
        let state = self.lock()?;
        Ok(state
            .datasets
            .values()
            .filter(|dataset| scope.map_or(true, |tenant| dataset.tenant == tenant))
            .map(|dataset| Self::dataset_view(&state, dataset))
            .collect())
    }

    /// One dataset with version summaries, or `None` (unknown or foreign).
    pub(crate) fn dataset(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
    ) -> Result<Option<DatasetView>> {
        let state = self.lock()?;
        Ok(state
            .dataset_in_scope(scope, dataset_id)
            .map(|dataset| Self::dataset_view(&state, dataset)))
    }

    fn dataset_view(state: &State, dataset: &Dataset) -> DatasetView {
        let versions = state
            .versions_by_dataset
            .get(&dataset.dataset_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.versions.get(id))
                    .map(|version| VersionSummary {
                        version_id: version.version_id.clone(),
                        parent: version.parent.clone(),
                        examples: version.examples.len(),
                        created_unix_ns: version.created_unix_ns,
                        tombstoned: state.tombstones.contains_key(&version.version_id),
                    })
                    .collect()
            })
            .unwrap_or_default();
        DatasetView {
            dataset: dataset.clone(),
            versions,
        }
    }

    /// One version with bodies, or the tombstone that hides it, or `None`.
    /// The `Err`-shaped middle exists so the HTTP layer can answer 410 with
    /// the tombstone rather than a bare 404.
    #[allow(clippy::type_complexity)]
    pub(crate) fn version(
        &self,
        scope: Option<&str>,
        dataset_id: u64,
        version_id: &str,
    ) -> Result<Option<std::result::Result<VersionView, VersionTombstone>>> {
        let state = self.lock()?;
        if state.dataset_in_scope(scope, dataset_id).is_none() {
            return Ok(None);
        }
        let Some(version) = state
            .versions
            .get(version_id)
            .filter(|version| version.dataset_id == dataset_id)
        else {
            return Ok(None);
        };
        if let Some(tombstone) = state.tombstones.get(version_id) {
            return Ok(Some(Err(tombstone.clone())));
        }
        let bodies = version
            .examples
            .iter()
            .filter_map(|(example_id, digest)| {
                state.examples.get(digest).map(|example| ExampleWithBody {
                    example_id: example_id.clone(),
                    digest: digest.clone(),
                    body: example.body.clone(),
                })
            })
            .collect();
        Ok(Some(Ok(VersionView {
            version: version.clone(),
            bodies,
        })))
    }

    /// Experiments visible under `scope`, optionally narrowed to a dataset.
    pub(crate) fn experiments(
        &self,
        scope: Option<&str>,
        dataset_id: Option<u64>,
    ) -> Result<Vec<ExperimentView>> {
        let state = self.lock()?;
        Ok(state
            .experiments
            .values()
            .filter(|experiment| scope.map_or(true, |tenant| experiment.tenant == tenant))
            .filter(|experiment| dataset_id.map_or(true, |id| experiment.dataset_id == id))
            .map(|experiment| Self::experiment_view(&state, experiment))
            .collect())
    }

    /// One experiment, or `None` (unknown or foreign).
    pub(crate) fn experiment(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
    ) -> Result<Option<ExperimentView>> {
        let state = self.lock()?;
        Ok(state
            .experiment_in_scope(scope, experiment_id)
            .map(|experiment| Self::experiment_view(&state, experiment)))
    }

    fn experiment_view(state: &State, experiment: &Experiment) -> ExperimentView {
        ExperimentView {
            experiment: experiment.clone(),
            dataset_version_deleted: state.tombstones.contains_key(&experiment.dataset_version),
            run_count: state
                .runs
                .iter()
                .filter(|run| run.experiment_id == experiment.experiment_id)
                .count(),
        }
    }

    /// An experiment's recorded runs, or `None` (unknown or foreign).
    pub(crate) fn runs(&self, scope: Option<&str>, experiment_id: u64) -> Result<Option<Vec<Run>>> {
        let state = self.lock()?;
        if state.experiment_in_scope(scope, experiment_id).is_none() {
            return Ok(None);
        }
        Ok(Some(
            state
                .runs
                .iter()
                .filter(|run| run.experiment_id == experiment_id)
                .cloned()
                .collect(),
        ))
    }

    /// The example ids of an experiment's dataset version, for summaries.
    pub(crate) fn experiment_examples(
        &self,
        scope: Option<&str>,
        experiment_id: u64,
    ) -> Result<Option<Vec<String>>> {
        let state = self.lock()?;
        let Some(experiment) = state.experiment_in_scope(scope, experiment_id) else {
            return Ok(None);
        };
        Ok(state
            .versions
            .get(&experiment.dataset_version)
            .map(|version| version.examples.iter().map(|(id, _)| id.clone()).collect()))
    }

    /// Every `$payload` reference any example body carries — unioned into
    /// the store's live set.
    pub(crate) fn payload_refs(&self) -> Result<HashSet<String>> {
        Ok(self.lock()?.payload_refs.clone())
    }

    /// Whether any of `tenant`'s dataset versions lists an example whose
    /// body carries `reference` — the eval half of a bound principal's
    /// payload-fetch reachability proof.
    pub(crate) fn tenant_references(&self, tenant: &str, reference: &str) -> Result<bool> {
        let state = self.lock()?;
        if !state.payload_refs.contains(reference) {
            return Ok(false);
        }
        for version in state.versions.values() {
            if version.tenant != tenant {
                continue;
            }
            for (_, digest) in &version.examples {
                let Some(example) = state.examples.get(digest) else {
                    continue;
                };
                let mut refs = HashSet::new();
                collect_refs(
                    &serde_json::to_value(&example.body).expect("body serializes"),
                    &mut refs,
                );
                if refs.contains(reference) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ---------------------------------------------------- erasure support

    /// The dataset and experiment ids a tenant owns — recorded into the
    /// erase record before the purge, for the receipt.
    pub(crate) fn ids_of_tenant(&self, tenant: &str) -> Result<(Vec<u64>, Vec<u64>)> {
        let state = self.lock()?;
        let datasets = state
            .datasets
            .values()
            .filter(|dataset| dataset.tenant == tenant)
            .map(|dataset| dataset.dataset_id)
            .collect();
        let experiments = state
            .experiments
            .values()
            .filter(|experiment| experiment.tenant == tenant)
            .map(|experiment| experiment.experiment_id)
            .collect();
        Ok((datasets, experiments))
    }

    /// Removes every record a tenant owns, rewriting the log atomically —
    /// the erasure-barrier rewrite, and the ONLY path that ever rewrites
    /// this log. Returns how many records were dropped. Example bodies
    /// survive when a surviving version still lists their digest (bodies
    /// are content-addressed and tenant-free); a counters record preserves
    /// id monotonicity past the rewrite.
    pub(crate) fn purge_tenant(&self, tenant: &str) -> Result<usize> {
        let mut state = self.lock()?;
        let doomed_datasets: HashSet<u64> = state
            .datasets
            .values()
            .filter(|dataset| dataset.tenant == tenant)
            .map(|dataset| dataset.dataset_id)
            .collect();
        let doomed_experiments: HashSet<u64> = state
            .experiments
            .values()
            .filter(|experiment| experiment.tenant == tenant)
            .map(|experiment| experiment.experiment_id)
            .collect();
        let doomed_versions: HashSet<String> = state
            .versions
            .values()
            .filter(|version| version.tenant == tenant)
            .map(|version| version.version_id.clone())
            .collect();
        let surviving_digests: HashSet<String> = state
            .versions
            .values()
            .filter(|version| version.tenant != tenant)
            .flat_map(|version| version.examples.iter().map(|(_, digest)| digest.clone()))
            .collect();
        let orphan_tombstones: usize = state
            .tombstones
            .values()
            .filter(|tombstone| tombstone.tenant == tenant)
            .count();
        let doomed_runs = state.runs.iter().filter(|run| run.tenant == tenant).count();
        let doomed_bodies = state
            .examples
            .keys()
            .filter(|digest| !surviving_digests.contains(*digest))
            .count();
        let removed = doomed_datasets.len()
            + doomed_experiments.len()
            + doomed_versions.len()
            + orphan_tombstones
            + doomed_runs
            + doomed_bodies;
        if removed == 0 {
            return Ok(0);
        }

        // Monotonicity floors OVER the pre-purge allocators: whatever ids
        // existed, erased or not, are never reissued.
        let mut survivors = State {
            dataset_floor: state.next_dataset_id().saturating_sub(1),
            experiment_floor: state.next_experiment_id().saturating_sub(1),
            ..State::default()
        };
        let mut records: Vec<LogRecord> = vec![LogRecord::Counters(Counters {
            schema: SCHEMA,
            dataset_next: survivors.dataset_floor,
            experiment_next: survivors.experiment_floor,
        })];
        for dataset in state.datasets.values() {
            if !doomed_datasets.contains(&dataset.dataset_id) {
                records.push(LogRecord::Dataset(dataset.clone()));
            }
        }
        for example in state.examples.values() {
            if surviving_digests.contains(&example.digest) {
                records.push(LogRecord::Example(example.clone()));
            }
        }
        for dataset_versions in state.versions_by_dataset.values() {
            for version_id in dataset_versions {
                if let Some(version) = state.versions.get(version_id) {
                    if !doomed_versions.contains(version_id) {
                        records.push(LogRecord::DatasetVersion(version.clone()));
                    }
                }
            }
        }
        for experiment in state.experiments.values() {
            if !doomed_experiments.contains(&experiment.experiment_id) {
                records.push(LogRecord::Experiment(experiment.clone()));
            }
        }
        for run in &state.runs {
            if run.tenant != tenant {
                records.push(LogRecord::Run(run.clone()));
            }
        }
        for tombstone in state.tombstones.values() {
            if tombstone.tenant != tenant {
                records.push(LogRecord::DatasetVersionTombstone(tombstone.clone()));
            }
        }

        // Staged, fsynced, renamed, directory synced — the annotation log's
        // rewrite discipline: a crash leaves the old log or the new one, and
        // the old one only errs toward a retry of the purge.
        let temp = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&temp)?;
            for record in &records {
                let mut line = serde_json::to_string(record)?;
                line.push('\n');
                file.write_all(line.as_bytes())?;
            }
            file.sync_all()?;
        }
        fs::rename(&temp, &self.path)?;
        if let Some(directory) = self.path.parent() {
            crate::sync_directory(directory)?;
        }
        for record in records {
            survivors.apply(record);
        }
        *state = survivors;
        Ok(removed)
    }

    /// How many records a tenant still owns — the receipt's eval domain for
    /// a tenant subject re-checks this after the purge.
    pub(crate) fn tenant_record_count(&self, tenant: &str) -> Result<usize> {
        let state = self.lock()?;
        let datasets = state
            .datasets
            .values()
            .filter(|dataset| dataset.tenant == tenant)
            .count();
        let versions = state
            .versions
            .values()
            .filter(|version| version.tenant == tenant)
            .count();
        let experiments = state
            .experiments
            .values()
            .filter(|experiment| experiment.tenant == tenant)
            .count();
        let runs = state.runs.iter().filter(|run| run.tenant == tenant).count();
        let tombstones = state
            .tombstones
            .values()
            .filter(|tombstone| tombstone.tenant == tenant)
            .count();
        Ok(datasets + versions + experiments + runs + tombstones)
    }

    /// The copies a trace/span/session subject may have left in ONE tenant's
    /// datasets: examples whose provenance names the subject's identifiers,
    /// or whose body content contains them. Scoped to the subject's tenant —
    /// walking another tenant's bodies into this receipt would hand the
    /// requester names it has no business seeing.
    pub(crate) fn copies_in_tenant(&self, tenant: &str, needles: &[String]) -> Result<Vec<String>> {
        let state = self.lock()?;
        let mut items = Vec::new();
        for version_ids in state.versions_by_dataset.values() {
            for version_id in version_ids {
                let Some(version) = state.versions.get(version_id) else {
                    continue;
                };
                if version.tenant != tenant {
                    continue;
                }
                for (example_id, digest) in &version.examples {
                    let Some(example) = state.examples.get(digest) else {
                        continue;
                    };
                    let provenance_hit = example.body.provenance.as_ref().is_some_and(|from| {
                        needles
                            .iter()
                            .any(|needle| from.trace_id == *needle || from.span_id == *needle)
                    });
                    let content_hit = || {
                        let rendered = canonical_json(
                            &serde_json::to_value(&example.body).expect("body serializes"),
                        );
                        needles.iter().any(|needle| rendered.contains(needle))
                    };
                    if provenance_hit || content_hit() {
                        items.push(format!(
                            "dataset {} version {} example {}",
                            version.dataset_id, version.version_id, example_id
                        ));
                    }
                }
            }
        }
        items.sort();
        items.dedup();
        Ok(items)
    }

    /// Every example (across ALL tenants — payload erasure is an operator
    /// act) whose body still references `reference`. These are DANGLING
    /// references after the blob's deletion: addresses, not content, and
    /// reported retained-by-design rather than flipping the receipt
    /// inconclusive.
    pub(crate) fn references_to(&self, reference: &str) -> Result<Vec<String>> {
        let state = self.lock()?;
        let mut holders: Vec<String> = Vec::new();
        let mut holding_digests: HashSet<&String> = HashSet::new();
        for (digest, example) in &state.examples {
            let mut refs = HashSet::new();
            collect_refs(
                &serde_json::to_value(&example.body).expect("body serializes"),
                &mut refs,
            );
            if refs.contains(reference) {
                holding_digests.insert(digest);
            }
        }
        if holding_digests.is_empty() {
            return Ok(holders);
        }
        for version_ids in state.versions_by_dataset.values() {
            for version_id in version_ids {
                let Some(version) = state.versions.get(version_id) else {
                    continue;
                };
                for (example_id, digest) in &version.examples {
                    if holding_digests.contains(digest) {
                        holders.push(format!(
                            "dataset {} version {} example {}",
                            version.dataset_id, version.version_id, example_id
                        ));
                    }
                }
            }
        }
        holders.sort();
        holders.dedup();
        Ok(holders)
    }
}

// ----------------------------------------------------- score aggregation

/// Distribution of one score name across an experiment's examples.
#[derive(Clone, Debug, Serialize)]
pub struct ScoreStat {
    /// The score name (`accuracy`, `groundedness`, …).
    pub name: String,
    /// Scores counted after LWW dedup — one per scored example.
    pub count: usize,
    /// Examples of the dataset version that have this score.
    pub examples_scored: usize,
    /// Mean of numeric values (booleans count as 0/1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    /// Minimum numeric value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Maximum numeric value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Median numeric value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    /// 95th-percentile numeric value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95: Option<f64>,
    /// Fraction of boolean scores that were `true`, when any were boolean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_rate: Option<f64>,
}

/// The distributions of every score name in one experiment.
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentSummary {
    /// The experiment.
    pub experiment_id: u64,
    /// Examples in its dataset version's manifest.
    pub examples_total: usize,
    /// Per-name distributions, sorted by name.
    pub scores: Vec<ScoreStat>,
}

/// One score name's movement between two experiments.
#[derive(Clone, Debug, Serialize)]
pub struct DiffStat {
    /// The score name.
    pub name: String,
    /// Mean in the base experiment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_base: Option<f64>,
    /// Mean in the candidate experiment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_candidate: Option<f64>,
    /// `mean_candidate - mean_base`, when both exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    /// Examples whose value went up (higher-is-better convention; booleans
    /// as 0/1 — documented, not configurable in this milestone).
    pub improved: Vec<String>,
    /// Examples whose value went down.
    pub regressed: Vec<String>,
    /// Examples scored in both with an unchanged value.
    pub unchanged: usize,
    /// Examples with this score only in the base experiment.
    pub only_base: Vec<String>,
    /// Examples with this score only in the candidate.
    pub only_candidate: Vec<String>,
}

/// An experiment-over-experiment comparison, joined on `(example_id, name)`.
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentDiff {
    /// The baseline experiment.
    pub base: u64,
    /// The candidate experiment.
    pub candidate: u64,
    /// Per-name movement, sorted by name.
    pub scores: Vec<DiffStat>,
}

/// One score annotation's value as a comparable number, when it has one.
/// Booleans map to 0/1 — that is what makes "pass" diffable — and numeric
/// strings are NOT coerced: a score is machine-written, and a writer that
/// stringifies numbers should hear about it early.
fn score_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        _ => None,
    }
}

/// The winning score per `(example_id, name)`: highest
/// `(timestamp_ns, trace_id, span_id)` wins — the store's last-write-wins
/// character applied to an append-only log, so a re-scored example counts
/// once and history stays in the log.
fn dedup_scores(
    scores: &[crate::annotations::Annotation],
) -> HashMap<(String, String), &crate::annotations::Annotation> {
    let mut latest: HashMap<(String, String), &crate::annotations::Annotation> = HashMap::new();
    for score in scores {
        let key = (score.example_id.clone(), score.name.clone());
        match latest.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(score);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let held = slot.get();
                let newer = (score.timestamp_ns, &score.trace_id, &score.span_id)
                    > (held.timestamp_ns, &held.trace_id, &held.span_id);
                if newer {
                    slot.insert(score);
                }
            }
        }
    }
    latest
}

/// Per-name distributions over one experiment's scores.
pub(crate) fn summarize_scores(
    experiment_id: u64,
    examples_total: usize,
    scores: &[crate::annotations::Annotation],
) -> ExperimentSummary {
    let latest = dedup_scores(scores);
    let mut by_name: BTreeMap<String, Vec<&crate::annotations::Annotation>> = BTreeMap::new();
    for ((_, name), score) in &latest {
        by_name.entry(name.clone()).or_default().push(score);
    }
    let scores = by_name
        .into_iter()
        .map(|(name, entries)| {
            let mut numeric: Vec<f64> = entries
                .iter()
                .filter_map(|score| score_value(&score.value))
                .collect();
            numeric.sort_by(f64::total_cmp);
            let booleans = entries
                .iter()
                .filter(|score| score.value.is_boolean())
                .count();
            let truthy = entries
                .iter()
                .filter(|score| score.value == Value::Bool(true))
                .count();
            let percentile = |fraction: f64| -> Option<f64> {
                if numeric.is_empty() {
                    return None;
                }
                let index = ((numeric.len() as f64 - 1.0) * fraction).round() as usize;
                numeric.get(index).copied()
            };
            ScoreStat {
                count: entries.len(),
                examples_scored: entries.len(),
                mean: match numeric.is_empty() {
                    true => None,
                    false => Some(numeric.iter().sum::<f64>() / numeric.len() as f64),
                },
                min: numeric.first().copied(),
                max: numeric.last().copied(),
                p50: percentile(0.50),
                p95: percentile(0.95),
                true_rate: match booleans {
                    0 => None,
                    total => Some(truthy as f64 / total as f64),
                },
                name,
            }
        })
        .collect();
    ExperimentSummary {
        experiment_id,
        examples_total,
        scores,
    }
}

/// The join of two experiments' scores on `(example_id, name)`.
pub(crate) fn diff_scores(
    base: u64,
    candidate: u64,
    base_scores: &[crate::annotations::Annotation],
    candidate_scores: &[crate::annotations::Annotation],
) -> ExperimentDiff {
    let base_latest = dedup_scores(base_scores);
    let candidate_latest = dedup_scores(candidate_scores);
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (_, name) in base_latest.keys().chain(candidate_latest.keys()) {
        names.insert(name.clone());
    }
    let scores = names
        .into_iter()
        .map(|name| {
            let side = |latest: &HashMap<(String, String), &crate::annotations::Annotation>| {
                latest
                    .iter()
                    .filter(|((_, held_name), _)| *held_name == name)
                    .map(|((example, _), score)| (example.clone(), score_value(&score.value)))
                    .collect::<BTreeMap<String, Option<f64>>>()
            };
            let base_values = side(&base_latest);
            let candidate_values = side(&candidate_latest);
            let mean = |values: &BTreeMap<String, Option<f64>>| {
                let numeric: Vec<f64> = values.values().filter_map(|value| *value).collect();
                match numeric.is_empty() {
                    true => None,
                    false => Some(numeric.iter().sum::<f64>() / numeric.len() as f64),
                }
            };
            let mean_base = mean(&base_values);
            let mean_candidate = mean(&candidate_values);
            let mut improved = Vec::new();
            let mut regressed = Vec::new();
            let mut unchanged = 0usize;
            let mut only_base = Vec::new();
            let mut only_candidate = Vec::new();
            for (example, base_value) in &base_values {
                match candidate_values.get(example) {
                    None => only_base.push(example.clone()),
                    Some(candidate_value) => match (base_value, candidate_value) {
                        (Some(before), Some(after)) if after > before => {
                            improved.push(example.clone())
                        }
                        (Some(before), Some(after)) if after < before => {
                            regressed.push(example.clone())
                        }
                        _ => unchanged += 1,
                    },
                }
            }
            for example in candidate_values.keys() {
                if !base_values.contains_key(example) {
                    only_candidate.push(example.clone());
                }
            }
            DiffStat {
                name,
                mean_base,
                mean_candidate,
                delta: match (mean_base, mean_candidate) {
                    (Some(before), Some(after)) => Some(after - before),
                    _ => None,
                },
                improved,
                regressed,
                unchanged,
                only_base,
                only_candidate,
            }
        })
        .collect();
    ExperimentDiff {
        base,
        candidate,
        scores,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The canonical form is pinned BYTE FOR BYTE, the way `hash128`'s
    /// digests and the segment layout are: these strings are persisted
    /// identity (example digests, version ids), and a formatting drift —
    /// serde_json changing float rendering, a map-order feature unifying in
    /// — must fail a test, not quietly re-address every dataset.
    #[test]
    fn canonical_json_is_pinned_byte_for_byte() {
        let value = json!({
            "z": 1,
            "a": {"nested": [1, 2.5, -3], "empty": {}},
            "s": "he\"llo\n\u{7}",
            "b": true,
            "n": null,
            "f": 0.1,
            "big": 18446744073709551615u64,
            "neg": -9223372036854775808i64,
        });
        assert_eq!(
            canonical_json(&value),
            "{\"a\":{\"empty\":{},\"nested\":[1,2.5,-3]},\"b\":true,\"big\":18446744073709551615,\
             \"f\":0.1,\"n\":null,\"neg\":-9223372036854775808,\"s\":\"he\\\"llo\\n\\u0007\",\"z\":1}"
        );
        // Key order is OURS, not the map's: a value built in reverse
        // insertion order canonicalizes identically.
        let reversed = json!({"a": 1, "z": 2});
        let forward = json!({"z": 2, "a": 1});
        assert_eq!(canonical_json(&reversed), canonical_json(&forward));
    }

    #[test]
    fn example_digests_are_stable_and_content_addressed() {
        let body = ExampleBody {
            input: json!({"prompt": "why"}),
            expected: Some(json!("because")),
            split: "test".into(),
            provenance: Some(Provenance {
                tenant: String::new(),
                trace_id: "t1".into(),
                span_id: "s1".into(),
            }),
        };
        // Pinned: this digest is persisted identity. If this test fails, the
        // canonical form changed and every stored dataset re-addresses —
        // that is a format break, not a refactor.
        assert_eq!(
            body.digest(),
            "63ff05e42ace5dc1757e0e1d1b8971130b652a1baa033e005ad7285cc10b9e28"
        );
        let same = ExampleBody {
            input: json!({"prompt": "why"}),
            expected: Some(json!("because")),
            split: "test".into(),
            provenance: Some(Provenance {
                tenant: String::new(),
                trace_id: "t1".into(),
                span_id: "s1".into(),
            }),
        };
        assert_eq!(body.digest(), same.digest());
        let different = ExampleBody {
            expected: Some(json!("beCause")),
            ..same
        };
        assert_ne!(body.digest(), different.digest());
    }
}
