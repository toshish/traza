//! Post-hoc span annotations: scores, human feedback, eval verdicts.
//!
//! Spans are immutable once ingested, but judgment about them arrives later —
//! a human thumbs-down, an eval score, a triage label. Annotations are a
//! separate record type in an append-only JSONL log (`annotations.jsonl` in
//! the data directory), fsync'd per append: their volume is human/eval scale,
//! orders of magnitude below span scale, so a flat log with an in-memory
//! index is the honest design. The TTL compactor rewrites the log dropping
//! entries older than the retention window.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// One annotation, addressed to a TYPED SUBJECT: a trace, a span within it,
/// a session, or an experiment example (a **score**).
///
/// The subject is expressed by which address fields are set, and exactly one
/// shape must hold:
///
/// - **trace** — `trace_id` alone (`span_id` empty);
/// - **span** — `trace_id` + `span_id`;
/// - **session** — `session_id` alone;
/// - **experiment example** — `experiment_id` + `example_id`, optionally
///   carrying `trace_id`/`span_id` naming the task run's span, which is what
///   makes a score address the `(experiment, example, span)` tuple.
///
/// The flat fields are the original wire shape, preserved: every pre-existing
/// record is a trace or span subject and decodes unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Annotation {
    /// Trace containing the annotated span. Required for trace/span subjects;
    /// optional on a score, where it names the run's trace.
    #[serde(default)]
    pub trace_id: String,
    /// Annotated span; empty annotates the trace as a whole.
    #[serde(default)]
    pub span_id: String,
    /// The tenant this annotation belongs to; empty is the default tenant.
    /// Scoped exactly like span identity — reads filter on it, erasure dooms
    /// by it. Accepts `$tenant` too, so a client that learned the span's
    /// reserved key cannot silently misroute a score to the default tenant by
    /// spelling it that way here; a closed schema has no ambiguity to protect
    /// against, only a keystroke to forgive.
    #[serde(alias = "$tenant", default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// Session-subject address: the recognized session id being judged as a
    /// whole. Mutually exclusive with the other subject shapes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    /// Experiment half of a score's address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<u64>,
    /// Example half of a score's address — the stable example id within the
    /// experiment's dataset version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub example_id: String,
    /// Annotation name (for example `quality`, `thumbs`, `groundedness`).
    pub name: String,
    /// Annotation value: number, string, bool — any JSON.
    pub value: Value,
    /// Who produced it (convention: `human:<who>` or `eval:<evaluator>`).
    #[serde(default)]
    pub source: String,
    /// Free-form comment.
    #[serde(default)]
    pub comment: String,
    /// When the annotation was recorded, nanoseconds since the Unix epoch.
    #[serde(default)]
    pub timestamp_ns: u64,
}

impl Annotation {
    /// Whether this annotation is a score — addressed to an experiment
    /// example. Scores are exempt from the TTL sweep: they live on eval
    /// retention, not trace retention, or an experiment-over-experiment diff
    /// would silently lose its base to a rolling window.
    pub fn is_score(&self) -> bool {
        self.experiment_id.is_some()
    }

    /// Validates the typed-subject shape; `Err` names the defect.
    pub(crate) fn validate_subject(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::InvalidSpan("annotation name is empty"));
        }
        if !self.tenant.is_empty() && !crate::valid_tenant(&self.tenant) {
            return Err(Error::InvalidSpan(
                "annotation tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes",
            ));
        }
        let has_trace = !self.trace_id.is_empty();
        let has_span = !self.span_id.is_empty();
        let has_session = !self.session_id.is_empty();
        let has_experiment = self.experiment_id.is_some();
        let has_example = !self.example_id.is_empty();
        if has_span && !has_trace {
            return Err(Error::InvalidSpan(
                "annotation span_id requires its trace_id",
            ));
        }
        if has_experiment != has_example {
            return Err(Error::InvalidSpan(
                "a score names both experiment_id and example_id",
            ));
        }
        if has_session && (has_trace || has_experiment) {
            return Err(Error::InvalidSpan(
                "a session annotation names only its session_id",
            ));
        }
        if !has_trace && !has_session && !has_experiment {
            return Err(Error::InvalidSpan(
                "annotation must address a trace, a span, a session, or an experiment example",
            ));
        }
        Ok(())
    }
}

/// Narrowings for an annotation search. Every supplied field must match; an
/// empty query returns everything, newest first.
#[derive(Clone, Debug, Default)]
pub struct AnnotationQuery<'a> {
    /// Restrict to one trace. Optional — scores are read across traces.
    pub trace_id: Option<&'a str>,
    /// Restrict to one span within the trace.
    pub span_id: Option<&'a str>,
    /// Restrict to one tenant. `None` is every tenant; `Some("")` the
    /// default tenant. A bound credential has this forced.
    pub tenant: Option<&'a str>,
    /// Restrict to session-subject annotations for this session id.
    pub session_id: Option<&'a str>,
    /// Restrict to scores of this experiment.
    pub experiment_id: Option<u64>,
    /// Restrict to scores of this example.
    pub example_id: Option<&'a str>,
    /// Restrict to one annotation name, for example `groundedness`.
    pub name: Option<&'a str>,
    /// Restrict to sources starting with this, so `human:` and `eval:`
    /// separate a review queue from a nightly run without an exact match.
    pub source_prefix: Option<&'a str>,
    /// Recorded at or after this Unix-nanosecond timestamp.
    pub since_ns: Option<u64>,
    /// Recorded at or before this Unix-nanosecond timestamp.
    pub until_ns: Option<u64>,
    /// Maximum returned, applied after ordering.
    pub limit: Option<usize>,
}

impl AnnotationQuery<'_> {
    fn matches(&self, annotation: &Annotation) -> bool {
        if self
            .span_id
            .is_some_and(|span_id| annotation.span_id != span_id)
        {
            return false;
        }
        if self
            .tenant
            .is_some_and(|tenant| annotation.tenant != tenant)
        {
            return false;
        }
        if self
            .session_id
            .is_some_and(|session| annotation.session_id != session)
        {
            return false;
        }
        if self
            .experiment_id
            .is_some_and(|experiment| annotation.experiment_id != Some(experiment))
        {
            return false;
        }
        if self
            .example_id
            .is_some_and(|example| annotation.example_id != example)
        {
            return false;
        }
        if self.name.is_some_and(|name| annotation.name != name) {
            return false;
        }
        if self
            .source_prefix
            .is_some_and(|prefix| !annotation.source.starts_with(prefix))
        {
            return false;
        }
        if self
            .since_ns
            .is_some_and(|since| annotation.timestamp_ns < since)
        {
            return false;
        }
        if self
            .until_ns
            .is_some_and(|until| annotation.timestamp_ns > until)
        {
            return false;
        }
        true
    }
}

const LOG_NAME: &str = "annotations.jsonl";

/// The append-only annotation log plus its in-memory trace index.
#[derive(Debug)]
pub(crate) struct AnnotationLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Every annotation, in log order — the single owner; the indexes below
    /// hold positions, not copies, so a corpus of machine-written scores is
    /// resident once.
    entries: Vec<Annotation>,
    /// Positions by trace id. Session- and experiment-subject annotations
    /// with no run trace live under the empty key.
    by_trace: HashMap<String, Vec<usize>>,
    /// Positions of scores, by experiment — the eval read path's index.
    by_experiment: HashMap<u64, Vec<usize>>,
}

impl Inner {
    fn adopt(&mut self, entries: Vec<Annotation>) {
        self.entries = entries;
        self.by_trace.clear();
        self.by_experiment.clear();
        for position in 0..self.entries.len() {
            self.index(position);
        }
    }

    fn index(&mut self, position: usize) {
        let annotation = &self.entries[position];
        self.by_trace
            .entry(annotation.trace_id.clone())
            .or_default()
            .push(position);
        if let Some(experiment) = annotation.experiment_id {
            self.by_experiment
                .entry(experiment)
                .or_default()
                .push(position);
        }
    }

    fn push(&mut self, annotation: Annotation) {
        self.entries.push(annotation);
        self.index(self.entries.len() - 1);
    }
}

impl AnnotationLog {
    /// Opens the log, replaying any existing entries into the index. A
    /// torn trailing line (crash mid-append) is ignored, matching the
    /// crash-consistency stance of the segment layer.
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
                match serde_json::from_slice::<Annotation>(line) {
                    Ok(annotation) => {
                        valid_len = valid_len.saturating_add(line.len() as u64);
                        inner.push(annotation);
                    }
                    // A crash can leave only the final append unterminated.
                    // A malformed newline-terminated record, or any malformed
                    // record before a later one, is real corruption: failing
                    // loudly prevents a valid suffix from disappearing.
                    Err(_) if lines.peek().is_none() && !terminated => {
                        // Heal the torn append before accepting new writes;
                        // otherwise the next valid JSON object would be
                        // concatenated onto this partial line.
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
                // A complete JSON object whose newline was the only torn byte
                // is valid. Restore the delimiter so the next append cannot
                // concatenate another object onto it.
                let mut file = OpenOptions::new().append(true).open(&path)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
        }
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// Appends one annotation durably (fsync) and indexes it. The subject
    /// shape is validated here, at the engine boundary, not only over HTTP.
    pub(crate) fn append(&self, annotation: Annotation) -> Result<()> {
        annotation.validate_subject()?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        let mut line = serde_json::to_string(&annotation)?;
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
        inner.push(annotation);
        Ok(())
    }

    /// All annotations for a trace, optionally narrowed to one span or name,
    /// scoped to `tenant` when given.
    pub(crate) fn query(
        &self,
        tenant: Option<&str>,
        trace_id: &str,
        span_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Vec<Annotation>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        Ok(inner
            .by_trace
            .get(trace_id)
            .map(|positions| {
                positions
                    .iter()
                    .map(|position| &inner.entries[*position])
                    .filter(|a| tenant.is_none() || tenant == Some(a.tenant.as_str()))
                    .filter(|a| span_id.is_none() || span_id == Some(a.span_id.as_str()))
                    .filter(|a| name.is_none() || name == Some(a.name.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Annotations matching every supplied narrowing, newest first.
    ///
    /// Unlike [`Self::query`] this does not require a trace: scores are
    /// produced per trace but read across them — an eval run is a population,
    /// not a lookup — and requiring `trace_id` meant the only way to see a
    /// run's results was to already know every trace in it. The index is fully
    /// in memory and annotation volume is human/eval scale, so the cross-trace
    /// path is a scan of that map rather than anything touching a segment.
    pub(crate) fn search(&self, narrow: &AnnotationQuery<'_>) -> Result<Vec<Annotation>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        let mut found: Vec<Annotation> = Vec::new();
        let mut consider = |annotation: &Annotation| {
            if narrow.matches(annotation) {
                found.push(annotation.clone());
            }
        };
        // Most selective index first: an experiment narrows to one run's
        // scores, a trace to one trace's judgments; only a fully open search
        // walks the population.
        if let Some(experiment) = narrow.experiment_id {
            for position in inner.by_experiment.get(&experiment).into_iter().flatten() {
                consider(&inner.entries[*position]);
            }
        } else if let Some(trace_id) = narrow.trace_id {
            for position in inner.by_trace.get(trace_id).into_iter().flatten() {
                consider(&inner.entries[*position]);
            }
        } else {
            for annotation in &inner.entries {
                consider(annotation);
            }
        }
        // Newest first, then a stable tiebreak so equal timestamps — which
        // a bulk eval import produces by the thousand — page deterministically.
        found.sort_by(|left, right| {
            right
                .timestamp_ns
                .cmp(&left.timestamp_ns)
                .then_with(|| left.tenant.cmp(&right.tenant))
                .then_with(|| left.trace_id.cmp(&right.trace_id))
                .then_with(|| left.span_id.cmp(&right.span_id))
                .then_with(|| left.example_id.cmp(&right.example_id))
                .then_with(|| left.name.cmp(&right.name))
        });
        if let Some(limit) = narrow.limit {
            found.truncate(limit);
        }
        Ok(found)
    }

    /// Drops annotations addressed to an erased subject, rewriting the log
    /// atomically (temp + rename). Returns how many were removed.
    ///
    /// The doom predicate mirrors the typed subject an annotation can carry:
    ///
    /// - `keys` — span-addressed annotations (scores' run addresses
    ///   included) whose `(tenant, trace_id, span_id)` was erased;
    /// - `whole_trace` — a trace subject, which also sweeps that trace's
    ///   trace-level annotations (`span_id` empty). A trace-level annotation
    ///   on a trace that was only PARTIALLY erased (a session cutting across
    ///   it) is deliberately kept: it is judgment about spans that still
    ///   exist;
    /// - `whole_session` — a session subject, which is what reaches
    ///   session-addressed annotations that carry no span address at all;
    /// - `whole_tenant` — a tenant subject: everything of the tenant's,
    ///   scores included, whatever its subject shape.
    pub(crate) fn drop_for_subject(
        &self,
        keys: &HashSet<(String, String, String)>,
        whole_trace: Option<(&str, &str)>,
        whole_session: Option<(&str, &str)>,
        whole_tenant: Option<&str>,
    ) -> Result<usize> {
        let doomed = |annotation: &Annotation| {
            whole_tenant.is_some_and(|tenant| annotation.tenant == tenant)
                || whole_trace.is_some_and(|(tenant, trace_id)| {
                    annotation.tenant == tenant && annotation.trace_id == trace_id
                })
                || whole_session.is_some_and(|(tenant, session_id)| {
                    annotation.tenant == tenant && annotation.session_id == session_id
                })
                || (!annotation.trace_id.is_empty()
                    && keys.contains(&(
                        annotation.tenant.clone(),
                        annotation.trace_id.clone(),
                        annotation.span_id.clone(),
                    )))
        };
        self.rewrite_keeping(|annotation| !doomed(annotation))
    }

    /// Drops annotations older than their tenant's cutoff by rewriting the
    /// log atomically (temp + rename). Returns how many were removed.
    ///
    /// `cutoff_for` maps a tenant to its cutoff; `None` means that tenant
    /// never expires. **Scores are exempt regardless**: they live on eval
    /// retention, not trace retention — a rolling window that swept January's
    /// scores would silently empty the base of every
    /// experiment-over-experiment diff run in March.
    pub(crate) fn drop_older_than(
        &self,
        cutoff_for: &dyn Fn(&str) -> Option<u64>,
    ) -> Result<usize> {
        self.rewrite_keeping(|annotation| {
            annotation.is_score()
                || cutoff_for(&annotation.tenant)
                    .map_or(true, |cutoff_ns| annotation.timestamp_ns >= cutoff_ns)
        })
    }

    /// Rewrites the log to the annotations `keep` admits — staged, fsynced,
    /// renamed, directory synced — so a crash leaves the old log or the new
    /// one, never a blend, and the old log only ever errs toward a retry.
    /// Returns how many were dropped; a no-op never touches the file.
    fn rewrite_keeping(&self, keep: impl Fn(&Annotation) -> bool) -> Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        let mut kept: Vec<Annotation> = Vec::new();
        let mut removed = 0;
        for annotation in &inner.entries {
            if keep(annotation) {
                kept.push(annotation.clone());
            } else {
                removed += 1;
            }
        }
        if removed == 0 {
            return Ok(0);
        }
        let temp = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&temp)?;
            for annotation in &kept {
                let mut line = serde_json::to_string(annotation)?;
                line.push('\n');
                file.write_all(line.as_bytes())?;
            }
            file.sync_all()?;
        }
        std::fs::rename(&temp, &self.path)?;
        if let Some(directory) = self.path.parent() {
            crate::sync_directory(directory)?;
        }
        inner.adopt(kept);
        Ok(removed)
    }
}
