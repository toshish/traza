//! Targeted deletion with a receipt.
//!
//! Expiry removes spans by age; erasure removes them by **subject** — a
//! trace, a span, a session, or an offloaded payload — because someone is
//! entitled to have them gone. The two obligations differ in what they must
//! prove: retention only has to happen, erasure has to be *demonstrable*.
//! This module holds the pieces that make it so:
//!
//! - [`Subject`], the thing being erased, resolved at request time to the
//!   concrete span keys it covers so the record of the erasure is exact
//!   rather than a predicate someone must re-evaluate later;
//! - the **tombstone log** (`tombstones.jsonl`), an append-only, fsynced,
//!   manifested record of every erasure requested and settled. It is a
//!   recovery domain like the annotation log: an erasure interrupted by a
//!   crash is found here and finished, never half-forgotten;
//! - [`Mask`], the in-memory view of erasures that are requested but not yet
//!   settled. Queries consult it so tombstoned content is never returned
//!   **even before the rewrite runs** — the window is normally the seconds
//!   inside one [`crate::Store::erase`] call, and after a crash it lasts
//!   until the resumed purge settles;
//! - the **receipt** ([`Receipt`]): `verify --erasure` re-checks every place
//!   the subject's bytes could be — write buffer, log, segments, annotation
//!   log, payload files, derived caches, pins — and reports the result in
//!   each, by name. A store with one recovery domain can enumerate its
//!   domains; that enumeration is the receipt, and the receipt is the point.
//!
//! **What the tombstone log itself retains, on purpose.** The record of an
//! erasure names its subject and the span keys it covered — identifiers, a
//! payload's content hash, counts; never the erased text. That is what makes
//! the receipt checkable at all, and it is stated in the receipt rather than
//! hidden: erasing the record of erasure means deleting the store.
//!
//! **Ordering discipline.** A new span ingested after an erasure settles is
//! new data, even under a recurring session id — a tombstone is a barrier,
//! not a ban. The receipt distinguishes the two cases exactly, because the
//! erase record carries the resolved keys: a key from the record found live
//! again is a **re-delivery** (some client replayed erased data; the receipt
//! fails and says so), while a fresh key matching the same subject is **new
//! activity**, reported informationally.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{payload, semconv, Error, Result, Span};

/// File name of the tombstone log, at the store root beside
/// `annotations.jsonl`. Manifested and append-only: generations digest it,
/// verification reads its recorded prefix, and appends past a manifest are
/// appends rather than damage.
pub(crate) const LOG_NAME: &str = "tombstones.jsonl";

/// Record schema version.
///
/// v2: tenancy — `span_keys` became `(tenant, trace, span)` triples, the
/// trace/span/session subjects grew a tenant field, and the tenant subject
/// kind exists. v1 records replay with the default tenant everywhere; this
/// field is the hinge that made that a decode rule instead of a guess.
const SCHEMA: u32 = 2;

/// What an erasure is about. Resolved once, at request time, to the concrete
/// span keys it covers (see [`EraseRecord::span_keys`]).
///
/// The `tenant` on trace/span/session subjects names the ONE tenant the
/// erasure targets; empty is the default tenant, never "all tenants". Two
/// tenants sharing a trace id are two subjects — erasing one leaves the
/// other untouched, which is the primary key doing its job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    /// Every span of one tenant's trace, its annotations, and any payload
    /// bytes no surviving span still references.
    Trace {
        /// The trace being erased.
        trace_id: String,
        /// Whose trace; empty is the default tenant.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tenant: String,
    },
    /// One span, by primary key.
    Span {
        /// Trace half of the primary key.
        trace_id: String,
        /// Span half of the primary key.
        span_id: String,
        /// Whose span; empty is the default tenant.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tenant: String,
    },
    /// Every span of one tenant that resolves to one session, across all
    /// recognized session keys (see [`crate::semconv`]).
    Session {
        /// The session identifier being erased.
        session_id: String,
        /// Whose session; empty is the default tenant.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tenant: String,
    },
    /// One offloaded payload by content address (`sha256/<hex>`). The file is
    /// deleted and every referencing span is rewritten with its inline
    /// preview redacted — the preview is content too. Content addressing is
    /// store-global, so this subject carries no tenant and is reserved for
    /// unbound (operator) admin credentials.
    Payload {
        /// The `sha256/<hex>` reference of the payload being erased.
        reference: String,
    },
    /// Everything one tenant owns: its spans across every domain, its
    /// annotations and scores, its datasets, versions, examples and
    /// experiments, and reference-aware deletion of the payload bytes its
    /// spans and examples held. The default tenant cannot be named here —
    /// erasing it is erasing the store, and narrower subjects exist.
    Tenant {
        /// The tenant being erased; must be non-empty.
        tenant: String,
    },
}

impl Subject {
    /// One line naming the subject, for logs and the receipt.
    pub fn describe(&self) -> String {
        let scope = |tenant: &str| {
            if tenant.is_empty() {
                String::new()
            } else {
                format!(" (tenant {tenant})")
            }
        };
        match self {
            Self::Trace { trace_id, tenant } => format!("trace {trace_id}{}", scope(tenant)),
            Self::Span {
                trace_id,
                span_id,
                tenant,
            } => format!("span {trace_id}/{span_id}{}", scope(tenant)),
            Self::Session { session_id, tenant } => {
                format!("session {session_id}{}", scope(tenant))
            }
            Self::Payload { reference } => format!("payload {reference}"),
            Self::Tenant { tenant } => format!("tenant {tenant}"),
        }
    }

    /// The tenant a scoped subject targets: `Some` for trace/span/session/
    /// tenant subjects (empty = default tenant), `None` for payload subjects,
    /// which are content-addressed and store-global.
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::Trace { tenant, .. }
            | Self::Span { tenant, .. }
            | Self::Session { tenant, .. } => Some(tenant),
            Self::Tenant { tenant } => Some(tenant),
            Self::Payload { .. } => None,
        }
    }

    /// The subject in its canonical form. Payload hashes are lowercased:
    /// stored references are lowercase hex and every comparison downstream is
    /// case-sensitive, so an uppercase request would match nothing in any
    /// span — and then read as a clean receipt over content that is still
    /// there. (On a case-insensitive filesystem the raw unlink WOULD have hit
    /// the real file, which is the worst of both: bytes gone, previews
    /// intact, receipt green. Canonicalizing at the boundary closes both.)
    pub fn canonicalized(self) -> Self {
        match self {
            Self::Payload { reference } => Self::Payload {
                reference: reference.to_ascii_lowercase(),
            },
            other => other,
        }
    }

    /// Whether the subject is well-formed enough to resolve: non-empty
    /// identifiers, admissible tenants, and a payload reference in its
    /// canonical form — lowercase hex, which [`Self::canonicalized`]
    /// produces.
    pub fn validate(&self) -> Result<()> {
        if let Some(tenant) = self.tenant() {
            if !tenant.is_empty() && !crate::valid_tenant(tenant) {
                return Err(Error::InvalidSpan(
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
                ));
            }
        }
        let problem = match self {
            Self::Trace { trace_id, .. } if trace_id.is_empty() => Some("trace_id is empty"),
            Self::Span {
                trace_id, span_id, ..
            } if trace_id.is_empty() || span_id.is_empty() => {
                Some("trace_id and span_id must both be non-empty")
            }
            Self::Session { session_id, .. } if session_id.is_empty() => {
                Some("session_id is empty")
            }
            Self::Payload { reference }
                if reference.strip_prefix("sha256/").map_or(true, |hash| {
                    hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                }) =>
            {
                Some("a payload reference is sha256/<64 lowercase hex characters>")
            }
            // The default tenant is every store that never configured
            // tenancy; "erase it whole" is "erase the store", which is not
            // an API. Narrower subjects express every legitimate deletion.
            Self::Tenant { tenant } if tenant.is_empty() => {
                Some("a tenant subject names a non-empty tenant")
            }
            _ => None,
        };
        match problem {
            Some(problem) => Err(Error::InvalidSpan(problem)),
            None => Ok(()),
        }
    }

    /// What the erasure does to a span: nothing, remove it, or rewrite it
    /// with the subject payload's inline preview redacted.
    pub(crate) fn action(&self, span: &Span) -> Action {
        match self {
            Self::Trace { trace_id, tenant } => {
                match span.trace_id == *trace_id && span.tenant == *tenant {
                    true => Action::Drop,
                    false => Action::Keep,
                }
            }
            Self::Span {
                trace_id,
                span_id,
                tenant,
            } => {
                match span.trace_id == *trace_id
                    && span.span_id == *span_id
                    && span.tenant == *tenant
                {
                    true => Action::Drop,
                    false => Action::Keep,
                }
            }
            Self::Session { session_id, tenant } => {
                match span.tenant == *tenant
                    && semconv::facts(&span.attributes).session.as_deref() == Some(session_id)
                {
                    true => Action::Drop,
                    false => Action::Keep,
                }
            }
            Self::Tenant { tenant } => match span.tenant == *tenant {
                true => Action::Drop,
                false => Action::Keep,
            },
            // Only an UNREDACTED reference draws a rewrite: the marker a
            // prior pass left behind is the erasure's end state, so acting on
            // it again would make resume rewrite segments it already fixed.
            Self::Payload { reference } => match payload_unredacted(span, reference) {
                true => Action::Redact,
                false => Action::Keep,
            },
        }
    }

    /// The payload reference this subject redacts, for [`Action::Redact`].
    pub(crate) fn payload_reference(&self) -> Option<&str> {
        match self {
            Self::Payload { reference } => Some(reference),
            _ => None,
        }
    }

    /// The identifier strings a byte-level occurrence scan looks for. Content
    /// hashes and trace ids are distinctive; a span id is only used when it is
    /// long enough not to drown the scan in coincidences, and always alongside
    /// its trace id.
    pub(crate) fn needles(&self) -> Vec<String> {
        match self {
            Self::Trace { trace_id, .. } => vec![trace_id.clone()],
            Self::Span {
                trace_id, span_id, ..
            } => {
                let mut needles = vec![trace_id.clone()];
                if span_id.len() >= 8 {
                    needles.push(span_id.clone());
                }
                needles
            }
            Self::Session { session_id, .. } => vec![session_id.clone()],
            Self::Payload { reference } => vec![reference.clone()],
            // The tenant name itself. Over-approximate like every needle —
            // another tenant embedding the same string in content can push
            // the count above zero, which reads as inconclusive, never as
            // wrong. Decode-walk domains classify tenant-exactly.
            Self::Tenant { tenant } => vec![tenant.clone()],
        }
    }
}

/// What an erasure does to one span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Untouched.
    Keep,
    /// Removed from every domain.
    Drop,
    /// Kept, with the subject payload's reference object rewritten to drop
    /// its inline preview.
    Redact,
}

/// Rewrites every unredacted reference to `reference` in `span`, dropping
/// the inline preview and marking the value erased. Returns whether anything
/// changed; a span already fully redacted changes nothing, which is what
/// makes a resumed erasure a no-op over work a prior pass finished.
pub(crate) fn redact_payload(span: &mut Span, reference: &str) -> bool {
    let mut changed = false;
    let mut redact_map = |attributes: &mut Map<String, Value>| {
        for value in attributes.values_mut() {
            let matches = value
                .get(payload::PAYLOAD_KEY)
                .and_then(Value::as_str)
                .is_some_and(|held| held == reference)
                && value.get("erased").and_then(Value::as_bool) != Some(true);
            if matches {
                let bytes = value.get("bytes").cloned().unwrap_or(Value::Null);
                let mut redacted = Map::new();
                redacted.insert(payload::PAYLOAD_KEY.into(), Value::String(reference.into()));
                redacted.insert("bytes".into(), bytes);
                redacted.insert("erased".into(), Value::Bool(true));
                *value = Value::Object(redacted);
                changed = true;
            }
        }
    };
    redact_map(&mut span.attributes);
    for event in &mut span.events {
        redact_map(&mut event.attributes);
    }
    changed
}

/// Every `sha256/<hex>` payload reference a span carries — excluding
/// redaction markers, whose bytes a prior erasure already removed. A marker
/// is the RECORD that content is gone; counting it as a reference would put
/// the erased bytes back under protection.
pub(crate) fn payload_refs_of(span: &Span, into: &mut HashSet<String>) {
    let mut collect = |attributes: &Map<String, Value>| {
        for value in attributes.values() {
            if value.get("erased").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(reference) = value.get(payload::PAYLOAD_KEY).and_then(Value::as_str) {
                into.insert(reference.to_owned());
            }
        }
    };
    collect(&span.attributes);
    for event in &span.events {
        collect(&event.attributes);
    }
}

// ------------------------------------------------------------- log records

/// One recorded span key: a schema-2 `(tenant, trace, span)` triple, or a
/// schema-1 `(trace, span)` pair that decodes as the default tenant. The
/// untagged enum is the whole replay-compatibility mechanism — a two-element
/// array cannot parse as a three-tuple, so arity decides, never a guess.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum RecordedKey {
    /// Schema 2: `(tenant, trace_id, span_id)`.
    Triple((String, String, String)),
    /// Schema 1: `(trace_id, span_id)`, default tenant.
    Pair((String, String)),
}

impl RecordedKey {
    fn into_triple(self) -> (String, String, String) {
        match self {
            Self::Triple(triple) => triple,
            Self::Pair((trace_id, span_id)) => (String::new(), trace_id, span_id),
        }
    }
}

/// The intent record: appended and fsynced BEFORE any byte is removed, so a
/// crash mid-erasure leaves a pending record the next open resumes rather
/// than a half-deletion nothing remembers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EraseRecord {
    /// Record schema version; see [`SCHEMA`].
    pub schema: u32,
    /// Monotonic erasure id, unique within this store.
    pub id: u64,
    /// Wall-clock request time, nanoseconds since the Unix epoch.
    pub requested_unix_ns: u64,
    /// What is being erased.
    pub subject: Subject,
    /// The concrete `(tenant, trace_id, span_id)` keys the subject resolved
    /// to at request time. This is the exactness the receipt rests on: a key
    /// from this list found live later is a re-delivery, a fresh key matching
    /// the subject is new activity, and no timestamp heuristic has to guess
    /// which. Bounded by the subject's own size — EXCEPT for tenant subjects,
    /// which deliberately record none: a tenant's key set is unbounded, the
    /// mask and the purge cover by predicate, and for a whole tenant the
    /// settle time IS the re-delivery line.
    #[serde(
        serialize_with = "serialize_keys",
        deserialize_with = "deserialize_keys"
    )]
    pub span_keys: Vec<(String, String, String)>,
    /// Payload references the covered spans carried (or, for a payload
    /// subject, the reference itself) — the set whose files the purge must
    /// account for, one disposition each, in the settle record. Empty for
    /// tenant subjects, whose refs the purge collects as it walks.
    pub payload_refs: Vec<String>,
    /// Dataset ids a tenant subject erased — recorded so the receipt can
    /// tell an erased dataset resurfacing (impossible: ids are never reused)
    /// from a new one. Bounded and small; empty for other subjects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eval_datasets: Vec<u64>,
    /// Experiment ids a tenant subject erased; see `eval_datasets`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eval_experiments: Vec<u64>,
}

fn serialize_keys<S: serde::Serializer>(
    keys: &[(String, String, String)],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_seq(keys)
}

fn deserialize_keys<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(String, String, String)>, D::Error> {
    let keys = Vec::<RecordedKey>::deserialize(deserializer)?;
    Ok(keys.into_iter().map(RecordedKey::into_triple).collect())
}

impl EraseRecord {
    /// A fresh intent record at the current schema version.
    pub(crate) fn new(
        id: u64,
        requested_unix_ns: u64,
        subject: Subject,
        span_keys: Vec<(String, String, String)>,
        payload_refs: Vec<String>,
        eval_datasets: Vec<u64>,
        eval_experiments: Vec<u64>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            id,
            requested_unix_ns,
            subject,
            span_keys,
            payload_refs,
            eval_datasets,
            eval_experiments,
        }
    }
}

/// A payload reference the purge deliberately did not delete, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetainedPayload {
    /// The `sha256/<hex>` reference.
    pub reference: String,
    /// Why its bytes remain: content addressing means the same bytes may back
    /// spans outside the subject, and reference-aware deletion must not
    /// destroy them.
    pub reason: String,
}

/// The outcome record: appended after the purge ran and the checkpoint
/// published. An erase record with no settle record is a pending erasure.
///
/// **The counts are the settling pass's counts, not lifetime totals.** A
/// crash between the physical purge and the settle append loses the first
/// pass's tallies with the process; the resumed pass finds the work already
/// done and settles with what IT removed — possibly zero. The receipt, not
/// this record, is the authority on absence: verification re-checks the
/// domains, never these numbers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettleRecord {
    /// Record schema version; see [`SCHEMA`].
    pub schema: u32,
    /// The erasure this settles.
    pub id: u64,
    /// Wall-clock settle time, nanoseconds since the Unix epoch.
    pub settled_unix_ns: u64,
    /// The generation whose `CURRENT` rename published the deletion.
    pub generation: u64,
    /// Spans removed by the settling pass across buffer, log, and segments
    /// (physical records, superseded versions included). See the type docs
    /// for what a crash-resumed settle reports.
    pub spans_removed: usize,
    /// Spans rewritten with a redacted payload reference (payload subjects),
    /// by the settling pass.
    pub spans_redacted: usize,
    /// Annotations removed from the annotation log by the settling pass.
    pub annotations_removed: usize,
    /// Payload files whose bytes were deleted (or already absent, on a
    /// resumed pass — absence is the erased state either way).
    pub payloads_removed: Vec<String>,
    /// Payload files deliberately kept, each with its reason.
    pub payloads_retained: Vec<RetainedPayload>,
    /// Eval records (datasets, versions, examples, experiments, runs,
    /// version tombstones) removed from the eval log by the settling pass —
    /// nonzero only for tenant subjects. A tenant's SCORES are annotations
    /// and count under `annotations_removed`.
    #[serde(default)]
    pub eval_records_removed: usize,
}

impl SettleRecord {
    /// The current schema version, for the store constructing a settle.
    pub(crate) fn schema_now() -> u32 {
        SCHEMA
    }
}

/// One line of the tombstone log.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LogRecord {
    /// See [`EraseRecord`].
    Erase(EraseRecord),
    /// See [`SettleRecord`].
    Settle(SettleRecord),
}

/// One erasure as the log currently knows it.
#[derive(Clone, Debug, Serialize)]
pub struct ErasureStatus {
    /// The intent record.
    #[serde(flatten)]
    pub erase: EraseRecord,
    /// The outcome, or `None` while the erasure is pending.
    pub settle: Option<SettleRecord>,
}

// -------------------------------------------------------------------- mask

/// The pending erasures' combined span coverage, consulted by every read
/// path. Cheap when empty — the store hands out `None` instead — and exact
/// when not: subjects that resolved to keys are checked by key, so a span
/// re-ingested under an erased key while its erasure is still pending stays
/// invisible, and one ingested after it settles is new data.
#[derive(Debug, Default)]
pub(crate) struct Mask {
    /// Resolved keys of every pending erasure, PLUS the subject key of every
    /// pending span-subject erasure — the subject is authoritative even when
    /// a hand-written or replayed record carries no resolved keys.
    keys: HashSet<(String, String, String)>,
    /// Pending trace subjects as `(tenant, trace_id)`, so spans of the trace
    /// that were not yet visible at resolve time (mid-flight batches) are
    /// covered too.
    traces: HashSet<(String, String)>,
    /// Pending session subjects as `(tenant, session_id)`, likewise.
    sessions: HashSet<(String, String)>,
    /// Pending tenant subjects: everything of theirs is covered by
    /// predicate, spans and annotations and eval records alike — a tenant's
    /// key set is unbounded, so no key list could do this job.
    tenants: HashSet<String>,
    /// Pending payload subjects: a span referencing one is masked whole
    /// until the redaction settles — over-hiding for seconds beats serving
    /// the preview of content someone asked to have erased.
    payloads: HashSet<String>,
    /// Every payload reference any pending erasure is due to account for —
    /// the subject reference plus the refs its spans carried. `GET
    /// /v1/payloads` withholds these bytes while the erasure is pending;
    /// a shared reference resurfaces at settle if live spans kept it.
    /// Deliberately NOT consulted by [`Self::covers`]: a popular shared
    /// payload (a system prompt) must not blank every span that carries it
    /// while an unrelated trace is being erased.
    payload_files: HashSet<String>,
}

impl Mask {
    /// Builds the mask for a set of pending erasures.
    pub(crate) fn for_pending(pending: &[EraseRecord]) -> Self {
        let mut mask = Self::default();
        for record in pending {
            mask.keys.extend(record.span_keys.iter().cloned());
            mask.payload_files
                .extend(record.payload_refs.iter().cloned());
            match &record.subject {
                Subject::Trace { trace_id, tenant } => {
                    mask.traces.insert((tenant.clone(), trace_id.clone()));
                }
                Subject::Session { session_id, tenant } => {
                    mask.sessions.insert((tenant.clone(), session_id.clone()));
                }
                Subject::Payload { reference } => {
                    mask.payloads.insert(reference.clone());
                    mask.payload_files.insert(reference.clone());
                }
                Subject::Span {
                    trace_id,
                    span_id,
                    tenant,
                } => {
                    // The subject IS the key. Relying on `span_keys` alone
                    // left the exact span visible whenever a record carried
                    // an empty list.
                    mask.keys
                        .insert((tenant.clone(), trace_id.clone(), span_id.clone()));
                }
                Subject::Tenant { tenant } => {
                    mask.tenants.insert(tenant.clone());
                }
            }
        }
        mask
    }

    /// Whether a pending erasure covers this span, for the READ paths: the
    /// drop set plus payload subjects, whose referencing spans are withheld
    /// whole until the redaction settles.
    pub(crate) fn covers(&self, span: &Span) -> bool {
        if self.covers_for_drop(span) {
            return true;
        }
        if !self.payloads.is_empty() {
            for reference in &self.payloads {
                if payload_unredacted(span, reference) {
                    return true;
                }
            }
        }
        false
    }

    /// Whether the ADMISSION barrier drops this span outright: keys, traces
    /// and sessions, but not payload subjects. A read may over-hide a span
    /// that references a pending payload for the seconds the erasure runs;
    /// dropping it at admission would destroy the span's unrelated data
    /// permanently. Admission redacts the doomed value instead — see
    /// [`Self::payload_subjects`].
    pub(crate) fn covers_for_drop(&self, span: &Span) -> bool {
        if !self.tenants.is_empty() && self.tenants.contains(&span.tenant) {
            return true;
        }
        if self
            .traces
            .contains(&(span.tenant.clone(), span.trace_id.clone()))
        {
            return true;
        }
        if self.keys.contains(&(
            span.tenant.clone(),
            span.trace_id.clone(),
            span.span_id.clone(),
        )) {
            return true;
        }
        if !self.sessions.is_empty() {
            if let Some(session) = semconv::facts(&span.attributes).session {
                if self.sessions.contains(&(span.tenant.clone(), session)) {
                    return true;
                }
            }
        }
        false
    }

    /// The payload references currently under erasure as SUBJECTS — the set
    /// admission redacts and offloading refuses to write bytes for.
    pub(crate) fn payload_subjects(&self) -> &HashSet<String> {
        &self.payloads
    }

    /// Whether a pending erasure covers this annotation, judged against the
    /// annotation's own typed subject: trace-level annotations (empty
    /// `span_id`) are covered by trace subjects, span addresses by any
    /// covered key, session-subject annotations by session subjects, and
    /// everything of a tenant's — scores included, whatever their shape —
    /// by a tenant subject. This is the annotate barrier's other half: an
    /// admission the purge could never reach by key must be refused by
    /// predicate.
    pub(crate) fn covers_annotation(&self, annotation: &crate::annotations::Annotation) -> bool {
        if !self.tenants.is_empty() && self.tenants.contains(&annotation.tenant) {
            return true;
        }
        if !annotation.trace_id.is_empty() {
            if self
                .traces
                .contains(&(annotation.tenant.clone(), annotation.trace_id.clone()))
            {
                return true;
            }
            if self.keys.contains(&(
                annotation.tenant.clone(),
                annotation.trace_id.clone(),
                annotation.span_id.clone(),
            )) {
                return true;
            }
        }
        if !annotation.session_id.is_empty()
            && self
                .sessions
                .contains(&(annotation.tenant.clone(), annotation.session_id.clone()))
        {
            return true;
        }
        false
    }

    /// Whether a pending TENANT erasure covers this tenant — the eval write
    /// paths' half of the barrier: a dataset, version, experiment, run or
    /// tombstone append for a tenant being erased must be refused, not
    /// raced.
    pub(crate) fn covers_tenant(&self, tenant: &str) -> bool {
        self.tenants.contains(tenant)
    }

    /// Whether a pending erasure is due to account for this payload's bytes.
    pub(crate) fn covers_payload_file(&self, reference: &str) -> bool {
        self.payload_files.contains(reference)
    }
}

// --------------------------------------------------------------------- log

/// The append-only tombstone log plus its replayed state.
#[derive(Debug)]
pub(crate) struct ErasureLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Every erasure ever recorded, by id, with its settle when one landed.
    records: BTreeMap<u64, (EraseRecord, Option<SettleRecord>)>,
    /// The mask over pending erasures, or `None` when nothing is pending —
    /// which is the common case and the fast path every query takes.
    mask: Option<Arc<Mask>>,
}

impl Inner {
    fn rebuild_mask(&mut self) {
        let pending: Vec<EraseRecord> = self
            .records
            .values()
            .filter(|(_, settle)| settle.is_none())
            .map(|(erase, _)| erase.clone())
            .collect();
        self.mask = match pending.is_empty() {
            true => None,
            false => Some(Arc::new(Mask::for_pending(&pending))),
        };
    }
}

impl ErasureLog {
    /// Opens the log, replaying existing records. A torn trailing line
    /// (crash mid-append) is healed exactly as the annotation log heals its
    /// own; a malformed interior record is corruption and fails the open.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut inner = Inner::default();
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
                    Ok(LogRecord::Erase(erase)) => {
                        valid_len = valid_len.saturating_add(line.len() as u64);
                        inner.records.insert(erase.id, (erase, None));
                    }
                    Ok(LogRecord::Settle(settle)) => {
                        valid_len = valid_len.saturating_add(line.len() as u64);
                        if let Some(entry) = inner.records.get_mut(&settle.id) {
                            entry.1 = Some(settle);
                        }
                        // A settle for an unknown id is tolerated: it can only
                        // be produced by a log manually spliced together, and
                        // dropping it is safer than inventing an intent.
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
        inner.rebuild_mask();
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| Error::LockPoisoned("erasures"))
    }

    fn append_line(&self, record: &LogRecord) -> Result<()> {
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

    /// Allocates the next id, records intent durably, and installs the
    /// erasure into the mask — one critical section, so two concurrent
    /// erasures can never claim the same id. After this returns, the erasure
    /// survives a crash and its subject is invisible to queries.
    pub(crate) fn begin(
        &self,
        requested_unix_ns: u64,
        subject: Subject,
        span_keys: Vec<(String, String, String)>,
        payload_refs: Vec<String>,
        eval_datasets: Vec<u64>,
        eval_experiments: Vec<u64>,
    ) -> Result<EraseRecord> {
        let mut inner = self.lock()?;
        let id = inner
            .records
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let erase = EraseRecord::new(
            id,
            requested_unix_ns,
            subject,
            span_keys,
            payload_refs,
            eval_datasets,
            eval_experiments,
        );
        self.append_line(&LogRecord::Erase(erase.clone()))?;
        inner.records.insert(erase.id, (erase.clone(), None));
        inner.rebuild_mask();
        Ok(erase)
    }

    /// Records a fully formed intent, for replay-shaped tests. The live path
    /// is [`Self::begin`], which also allocates the id.
    #[cfg(test)]
    pub(crate) fn record_erase(&self, erase: EraseRecord) -> Result<()> {
        let mut inner = self.lock()?;
        self.append_line(&LogRecord::Erase(erase.clone()))?;
        inner.records.insert(erase.id, (erase, None));
        inner.rebuild_mask();
        Ok(())
    }

    /// Records the outcome durably and lifts the erasure out of the mask.
    pub(crate) fn record_settle(&self, settle: SettleRecord) -> Result<()> {
        let mut inner = self.lock()?;
        self.append_line(&LogRecord::Settle(settle.clone()))?;
        if let Some(entry) = inner.records.get_mut(&settle.id) {
            entry.1 = Some(settle);
        }
        inner.rebuild_mask();
        Ok(())
    }

    /// The pending-erasure mask, or `None` when nothing is pending.
    pub(crate) fn mask(&self) -> Option<Arc<Mask>> {
        self.lock().ok().and_then(|inner| inner.mask.clone())
    }

    /// Erasures whose purge has not settled, oldest first.
    pub(crate) fn pending(&self) -> Result<Vec<EraseRecord>> {
        let inner = self.lock()?;
        Ok(inner
            .records
            .values()
            .filter(|(_, settle)| settle.is_none())
            .map(|(erase, _)| erase.clone())
            .collect())
    }

    /// Every erasure the log knows, oldest first.
    pub(crate) fn list(&self) -> Result<Vec<ErasureStatus>> {
        let inner = self.lock()?;
        Ok(inner
            .records
            .values()
            .map(|(erase, settle)| ErasureStatus {
                erase: erase.clone(),
                settle: settle.clone(),
            })
            .collect())
    }

    /// One erasure by id.
    pub(crate) fn get(&self, id: u64) -> Result<Option<ErasureStatus>> {
        let inner = self.lock()?;
        Ok(inner.records.get(&id).map(|(erase, settle)| ErasureStatus {
            erase: erase.clone(),
            settle: settle.clone(),
        }))
    }
}

// ----------------------------------------------------------------- receipt

/// One domain's verification result.
#[derive(Clone, Debug, Serialize)]
pub struct DomainReport {
    /// The domain checked, by name.
    pub domain: String,
    /// `clear`, `holds-data`, `attention`, `retained-by-design`, or
    /// `not-applicable`.
    pub result: String,
    /// What was checked and what was found, in one sentence.
    pub detail: String,
    /// Erased keys found live again — re-delivered data. Any nonzero count
    /// fails the receipt.
    #[serde(skip_serializing_if = "is_zero")]
    pub re_delivered: usize,
    /// Spans matching the subject under keys the erasure never covered —
    /// data ingested after the fact. Informational, never a failure.
    #[serde(skip_serializing_if = "is_zero")]
    pub new_activity: usize,
    /// Raw byte-level occurrences of the subject's identifiers, where the
    /// domain is checked by occurrence scan rather than by decoding.
    #[serde(skip_serializing_if = "is_zero")]
    pub occurrences: usize,
    /// Named findings: a pin that holds data, a payload retained and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl DomainReport {
    pub(crate) fn clear(domain: &str, detail: String) -> Self {
        Self {
            domain: domain.to_owned(),
            result: "clear".to_owned(),
            detail,
            re_delivered: 0,
            new_activity: 0,
            occurrences: 0,
            items: Vec::new(),
        }
    }
}

/// The erasure receipt: every domain the subject's bytes could inhabit,
/// checked by name, with the result of each. Produced by
/// [`crate::Store::verify_erasure`] and the `verify --erasure` subcommand.
#[derive(Clone, Debug, Serialize)]
pub struct Receipt {
    /// The erasure being verified.
    pub erasure_id: u64,
    /// Its subject.
    pub subject: Subject,
    /// When the erasure was requested, nanoseconds since the Unix epoch.
    pub requested_unix_ns: u64,
    /// When this verification ran, nanoseconds since the Unix epoch.
    pub verified_unix_ns: u64,
    /// The generation that published the deletion, when it has settled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Whether the erasure has settled at all. An unsettled erasure never
    /// verifies as erased.
    pub settled: bool,
    /// Per-domain results, in a stable order.
    pub domains: Vec<DomainReport>,
    /// `erased` when every SEMANTIC check is clear or retained for a stated
    /// reason; `incomplete` otherwise. The receipt is a verification, not a
    /// claim: this field is computed from the domain results and nothing
    /// else.
    pub result: String,
    /// Whether the verification is also free of unexplained over-approximate
    /// signals: `false` whenever a byte-level occurrence scan found the
    /// subject's identifiers, whose matches CAN be benign (an identifier
    /// quoted in unrelated content) but were not proven so. `erased` answers
    /// what the semantic walk found; this answers whether anything at all
    /// was left ambiguous. A receipt offered as proof should carry both, and
    /// the subcommand's exit code distinguishes them.
    pub conclusive: bool,
}

impl Receipt {
    /// Renders the receipt as the human-readable report the subcommand
    /// prints. The JSON form is the artifact; this is the summary an
    /// operator reads first.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "erasure {} — {}\nrequested {} ns · settled: {}{}\n\n",
            self.erasure_id,
            self.subject.describe(),
            self.requested_unix_ns,
            match self.settled {
                true => "yes",
                false => "NO — purge has not completed",
            },
            self.generation
                .map(|generation| format!(" · published in generation {generation}"))
                .unwrap_or_default(),
        ));
        for domain in &self.domains {
            out.push_str(&format!(
                "  {:<24} {:<20} {}\n",
                domain.domain, domain.result, domain.detail
            ));
            for item in &domain.items {
                out.push_str(&format!("  {:<24} {:<20}   - {item}\n", "", ""));
            }
        }
        out.push_str(&format!(
            "\nresult: {}{}\n",
            self.result,
            match (self.result.as_str(), self.conclusive) {
                ("erased", false) =>
                    " (INCONCLUSIVE: occurrence scans found the subject's \
                     identifiers; see the attention domains above)",
                _ => "",
            }
        ));
        out
    }
}

/// Streaming count of byte-level occurrences of any of `needles` in the file
/// at `path`. Over-approximate on purpose — an identifier quoted inside
/// unrelated text still counts — because for a verification the safe error
/// is a finding that turns out benign, never a miss. A missing file counts
/// zero: absent bytes hold nothing.
pub(crate) fn count_occurrences(path: &Path, needles: &[String]) -> Result<usize> {
    let longest = needles.iter().map(String::len).max().unwrap_or(0);
    if longest == 0 {
        return Ok(0);
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(Error::Io(error)),
    };
    let mut count = 0usize;
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        // The carried tail seeds this window, so anything counted entirely
        // inside it last round is about to be counted again — remove it
        // FIRST. Subtracting after the count instead lost an occurrence
        // sitting in the file's final tail: counted once, subtracted once,
        // and never recounted because no next window came.
        for needle in needles {
            count = count.saturating_sub(count_in(&carry, needle.as_bytes()));
        }
        let mut window = std::mem::take(&mut carry);
        window.extend_from_slice(&chunk[..read]);
        for needle in needles {
            count += count_in(&window, needle.as_bytes());
        }
        // Keep enough tail that a needle split across the chunk boundary is
        // seen whole by the next window — and only the tail, so memory stays
        // constant however large the file is.
        let keep = longest.saturating_sub(1).min(window.len());
        carry = window[window.len() - keep..].to_vec();
    }
    Ok(count)
}

fn count_in(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut from = 0;
    while from + needle.len() <= haystack.len() {
        match haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        {
            Some(at) => {
                count += 1;
                from += at + 1;
            }
            None => break,
        }
    }
    count
}

/// Classifies live spans found matching the subject: keys the erase record
/// covered are re-deliveries, fresh keys are new activity.
pub(crate) struct MatchClasses {
    /// Erased keys found live again.
    pub re_delivered: usize,
    /// Subject-matching spans under keys the erasure never covered.
    pub new_activity: usize,
}

/// Splits `matches` against the erase record's resolved keys.
pub(crate) fn classify_matches(
    erased_keys: &HashSet<(String, String, String)>,
    matches: impl IntoIterator<Item = (String, String, String)>,
) -> MatchClasses {
    let mut classes = MatchClasses {
        re_delivered: 0,
        new_activity: 0,
    };
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    for key in matches {
        if !seen.insert(key.clone()) {
            continue;
        }
        match erased_keys.contains(&key) {
            true => classes.re_delivered += 1,
            false => classes.new_activity += 1,
        }
    }
    classes
}

/// For a payload subject: whether the span still carries an UNREDACTED
/// reference (a preview or any sibling field beyond the redaction marker's
/// own). A redacted marker left in place is the erasure working as designed.
pub(crate) fn payload_unredacted(span: &Span, reference: &str) -> bool {
    let unredacted = |value: &Value| {
        value
            .get(payload::PAYLOAD_KEY)
            .and_then(Value::as_str)
            .is_some_and(|held| held == reference)
            && value.get("erased").and_then(Value::as_bool) != Some(true)
    };
    span.attributes.values().any(unredacted)
        || span
            .events
            .iter()
            .any(|event| event.attributes.values().any(unredacted))
}

/// The pins directory's labels, for the receipt's pin walk.
pub(crate) fn pin_labels(directory: &Path) -> Result<Vec<String>> {
    let pins = directory.join("pins");
    let mut labels = Vec::new();
    let entries = match fs::read_dir(&pins) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(labels),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && !name.starts_with('.') {
            labels.push(name);
        }
    }
    labels.sort();
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "traza-erasure-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn erase_record(id: u64, subject: Subject) -> EraseRecord {
        EraseRecord {
            schema: SCHEMA,
            id,
            requested_unix_ns: 1,
            subject,
            span_keys: vec![(String::new(), "t1".into(), "s1".into())],
            payload_refs: Vec::new(),
            eval_datasets: Vec::new(),
            eval_experiments: Vec::new(),
        }
    }

    #[test]
    fn the_log_round_trips_and_pending_is_erase_without_settle() {
        let dir = temp_dir("roundtrip");
        let log = ErasureLog::open(&dir).expect("opens");
        let begun = log
            .begin(
                1,
                Subject::Trace {
                    trace_id: "t1".into(),
                    tenant: String::new(),
                },
                vec![(String::new(), "t1".into(), "s1".into())],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("erase");
        assert_eq!(begun.id, 1, "the first erasure takes id 1");
        assert_eq!(log.pending().expect("pending").len(), 1);
        assert!(log.mask().is_some(), "a pending erasure masks");

        log.record_settle(SettleRecord {
            schema: SCHEMA,
            id: 1,
            settled_unix_ns: 2,
            generation: 3,
            spans_removed: 1,
            spans_redacted: 0,
            annotations_removed: 0,
            payloads_removed: Vec::new(),
            payloads_retained: Vec::new(),
            eval_records_removed: 0,
        })
        .expect("settle");
        assert!(log.pending().expect("pending").is_empty());
        assert!(log.mask().is_none(), "a settled erasure stops masking");

        // Replay reproduces the same state, and the id counter resumes past
        // every recorded id.
        drop(log);
        let log = ErasureLog::open(&dir).expect("reopens");
        assert!(log.pending().expect("pending").is_empty());
        let next = log
            .begin(
                2,
                Subject::Trace {
                    trace_id: "t2".into(),
                    tenant: String::new(),
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("erase");
        assert_eq!(next.id, 2, "ids resume past every recorded id");
        let listed = log.list().expect("list");
        assert_eq!(listed.len(), 2);
        assert!(listed[0].settle.is_some());
        assert!(listed[1].settle.is_none());
    }

    #[test]
    fn a_torn_trailing_append_heals_and_an_interior_tear_refuses() {
        let dir = temp_dir("torn");
        let log = ErasureLog::open(&dir).expect("opens");
        log.record_erase(erase_record(
            1,
            Subject::Trace {
                trace_id: "t1".into(),
                tenant: String::new(),
            },
        ))
        .expect("erase");
        drop(log);

        let path = dir.join(LOG_NAME);
        let mut bytes = fs::read(&path).expect("read");
        bytes.extend_from_slice(b"{\"op\":\"erase\",\"schema\":1,\"id\":2");
        fs::write(&path, &bytes).expect("write");
        let log = ErasureLog::open(&dir).expect("heals the torn tail");
        assert_eq!(log.list().expect("list").len(), 1);
        drop(log);

        // Interior damage is corruption, not a tail.
        let mut bytes = fs::read(&path).expect("read");
        bytes.splice(0..1, b"X".iter().copied());
        fs::write(&path, &bytes).expect("write");
        assert!(ErasureLog::open(&dir).is_err());
    }

    #[test]
    fn the_mask_covers_by_key_trace_session_and_payload() {
        let mut by_key = erase_record(
            1,
            Subject::Span {
                trace_id: "t1".into(),
                span_id: "s1".into(),
                tenant: String::new(),
            },
        );
        by_key.span_keys = vec![(String::new(), "t1".into(), "s1".into())];
        let mut by_session = erase_record(
            2,
            Subject::Session {
                session_id: "sess-9".into(),
                tenant: String::new(),
            },
        );
        by_session.span_keys = Vec::new();
        let mask = Mask::for_pending(&[by_key, by_session]);

        let mut covered: Span = serde_json::from_value(serde_json::json!({
            "trace_id": "t1", "span_id": "s1", "name": "n", "service": "svc",
            "start_time_ns": 1u64, "end_time_ns": 2u64,
        }))
        .expect("span");
        assert!(mask.covers(&covered));
        covered.span_id = "s2".into();
        assert!(!mask.covers(&covered));
        covered
            .attributes
            .insert("session.id".into(), Value::String("sess-9".into()));
        assert!(mask.covers(&covered), "session subjects mask by resolution");
    }

    #[test]
    fn a_span_subject_masks_from_its_subject_even_with_no_resolved_keys() {
        // A replayed or hand-written record may carry an empty key list; the
        // subject itself must still mask, or the exact span someone asked to
        // erase stays visible for the whole pending window.
        let mut record = erase_record(
            1,
            Subject::Span {
                trace_id: "t1".into(),
                span_id: "s1".into(),
                tenant: String::new(),
            },
        );
        record.span_keys = Vec::new();
        let mask = Mask::for_pending(&[record]);
        let span: Span = serde_json::from_value(serde_json::json!({
            "trace_id": "t1", "span_id": "s1", "name": "n", "service": "svc",
            "start_time_ns": 1u64, "end_time_ns": 2u64,
        }))
        .expect("span");
        assert!(mask.covers(&span));
    }

    #[test]
    fn the_mask_covers_annotations_and_payload_files() {
        let trace = erase_record(
            1,
            Subject::Trace {
                trace_id: "t1".into(),
                tenant: String::new(),
            },
        );
        let reference = format!("sha256/{}", "c".repeat(64));
        let mut with_refs = erase_record(
            2,
            Subject::Span {
                trace_id: "t9".into(),
                span_id: "s9".into(),
                tenant: String::new(),
            },
        );
        with_refs.span_keys = vec![(String::new(), "t9".into(), "s9".into())];
        with_refs.payload_refs = vec![reference.clone()];
        let mask = Mask::for_pending(&[trace, with_refs]);

        let annotation = |trace_id: &str, span_id: &str| crate::annotations::Annotation {
            trace_id: trace_id.to_owned(),
            span_id: span_id.to_owned(),
            tenant: String::new(),
            session_id: String::new(),
            experiment_id: None,
            example_id: String::new(),
            name: "note".into(),
            value: Value::Bool(true),
            source: String::new(),
            comment: String::new(),
            timestamp_ns: 0,
        };
        assert!(
            mask.covers_annotation(&annotation("t1", "")),
            "trace-level annotation"
        );
        assert!(
            mask.covers_annotation(&annotation("t1", "any")),
            "span under the trace"
        );
        assert!(
            mask.covers_annotation(&annotation("t9", "s9")),
            "covered key"
        );
        assert!(!mask.covers_annotation(&annotation("t9", "other")));
        assert!(
            mask.covers_payload_file(&reference),
            "a payload the erasure must account for is withheld while pending"
        );
        assert!(!mask.covers_payload_file("sha256/absent"));
    }

    #[test]
    fn payload_ref_collection_skips_redaction_markers() {
        let reference = format!("sha256/{}", "d".repeat(64));
        let span: Span = serde_json::from_value(serde_json::json!({
            "trace_id": "t", "span_id": "s", "name": "n", "service": "svc",
            "start_time_ns": 1u64, "end_time_ns": 2u64,
            "attributes": {
                "gone": {"$payload": reference, "bytes": 9, "erased": true},
                "held": {"$payload": format!("sha256/{}", "e".repeat(64)), "bytes": 9,
                         "preview": "still here"},
            },
        }))
        .expect("span");
        let mut refs = HashSet::new();
        payload_refs_of(&span, &mut refs);
        assert!(
            !refs.contains(&reference),
            "a marker records that content is gone; it is not a reference"
        );
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn redaction_drops_the_preview_and_marks_the_value() {
        let reference = format!("sha256/{}", "a".repeat(64));
        let mut span: Span = serde_json::from_value(serde_json::json!({
            "trace_id": "t", "span_id": "s", "name": "n", "service": "svc",
            "start_time_ns": 1u64, "end_time_ns": 2u64,
            "attributes": {
                "prompt": {"$payload": reference, "bytes": 9000, "preview": "the secret text"},
                "other": "kept"
            },
        }))
        .expect("span");
        assert!(payload_unredacted(&span, &reference));
        assert!(redact_payload(&mut span, &reference));
        let value = span.attributes.get("prompt").expect("value");
        assert!(value.get("preview").is_none(), "the preview is content");
        assert_eq!(value.get("erased"), Some(&Value::Bool(true)));
        assert!(!payload_unredacted(&span, &reference));
        assert!(
            !redact_payload(&mut span, &reference),
            "redaction is idempotent"
        );
    }

    #[test]
    fn occurrence_counting_is_exact_across_chunk_boundaries() {
        let dir = temp_dir("occurrences");
        let path = dir.join("bytes.bin");
        // Straddle the 256 KiB chunk boundary deliberately.
        let mut bytes = vec![b'x'; 256 * 1024 - 3];
        bytes.extend_from_slice(b"needle-alpha");
        bytes.extend(vec![b'y'; 100]);
        bytes.extend_from_slice(b"needle-alpha");
        fs::write(&path, &bytes).expect("write");
        let count = count_occurrences(&path, &["needle-alpha".to_owned()]).expect("counts");
        assert_eq!(count, 2);
        assert_eq!(
            count_occurrences(&dir.join("absent.bin"), &["x".to_owned()]).expect("counts"),
            0,
            "absent bytes hold nothing"
        );

        // A SHORT needle sitting in the file's final tail, beside a longer
        // one that sets the carry size: the short needle fits entirely inside
        // the carried bytes, which is the case where subtract-after-count
        // silently lost the last occurrence.
        let path = dir.join("tail.bin");
        let mut bytes = vec![b'x'; 100];
        bytes.extend_from_slice(b"ab");
        fs::write(&path, &bytes).expect("write");
        let count = count_occurrences(&path, &["ab".to_owned(), "needle-alpha".to_owned()])
            .expect("counts");
        assert_eq!(count, 1, "an occurrence in the final tail counts once");
    }

    #[test]
    fn subjects_validate_and_classify() {
        assert!(Subject::Trace {
            trace_id: String::new(),
            tenant: String::new(),
        }
        .validate()
        .is_err());
        assert!(
            Subject::Tenant {
                tenant: String::new()
            }
            .validate()
            .is_err(),
            "the default tenant is not erasable whole — that is the store"
        );
        assert!(
            Subject::Trace {
                trace_id: "t".into(),
                tenant: "Not-Valid".into(),
            }
            .validate()
            .is_err(),
            "subject tenants obey the same charset ingest enforces"
        );
        assert!(Subject::Tenant {
            tenant: "acme".into()
        }
        .validate()
        .is_ok());
        assert!(Subject::Payload {
            reference: "sha256/short".into()
        }
        .validate()
        .is_err());
        assert!(Subject::Payload {
            reference: format!("sha256/{}", "b".repeat(64))
        }
        .validate()
        .is_ok());
        // Uppercase hex is not the canonical form: stored references are
        // lowercase and every comparison is case-sensitive, so accepting it
        // produced a green receipt over untouched content (and, on a
        // case-insensitive filesystem, an unlink of the REAL file besides).
        let uppercase = Subject::Payload {
            reference: format!("sha256/{}", "B".repeat(64)),
        };
        assert!(uppercase.validate().is_err());
        let canonical = uppercase.canonicalized();
        assert!(canonical.validate().is_ok());
        assert_eq!(
            canonical,
            Subject::Payload {
                reference: format!("sha256/{}", "b".repeat(64))
            }
        );

        let erased: HashSet<(String, String, String)> =
            [(String::new(), "t1".to_owned(), "s1".to_owned())]
                .into_iter()
                .collect();
        let classes = classify_matches(
            &erased,
            vec![
                (String::new(), "t1".to_owned(), "s1".to_owned()),
                (String::new(), "t1".to_owned(), "s1".to_owned()),
                (String::new(), "t9".to_owned(), "s9".to_owned()),
            ],
        );
        assert_eq!(classes.re_delivered, 1, "duplicates count once");
        assert_eq!(classes.new_activity, 1);
    }
}
