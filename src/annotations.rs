//! Post-hoc span annotations: scores, human feedback, eval verdicts.
//!
//! Spans are immutable once ingested, but judgment about them arrives later —
//! a human thumbs-down, an eval score, a triage label. Annotations are a
//! separate record type in an append-only JSONL log (`annotations.jsonl` in
//! the data directory), fsync'd per append: their volume is human/eval scale,
//! orders of magnitude below span scale, so a flat log with an in-memory
//! index is the honest design. The TTL compactor rewrites the log dropping
//! entries older than the retention window.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// One annotation attached to a span (or to a whole trace when `span_id`
/// is empty).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Annotation {
    /// Trace containing the annotated span.
    pub trace_id: String,
    /// Annotated span; empty string annotates the trace as a whole.
    #[serde(default)]
    pub span_id: String,
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

const LOG_NAME: &str = "annotations.jsonl";

/// The append-only annotation log plus its in-memory trace index.
#[derive(Debug)]
pub(crate) struct AnnotationLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    by_trace: HashMap<String, Vec<Annotation>>,
    count: usize,
}

impl AnnotationLog {
    /// Opens the log, replaying any existing entries into the index. A
    /// torn trailing line (crash mid-append) is ignored, matching the
    /// crash-consistency stance of the segment layer.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut inner = Inner::default();
        if path.exists() {
            let reader = BufReader::new(File::open(&path)?);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Annotation>(&line) {
                    Ok(annotation) => {
                        inner.count += 1;
                        inner
                            .by_trace
                            .entry(annotation.trace_id.clone())
                            .or_default()
                            .push(annotation);
                    }
                    Err(_) => break, // torn tail: everything before it stands
                }
            }
        }
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// Appends one annotation durably (fsync) and indexes it.
    pub(crate) fn append(&self, annotation: Annotation) -> Result<()> {
        if annotation.trace_id.is_empty() {
            return Err(Error::InvalidSpan("annotation trace_id is empty"));
        }
        if annotation.name.is_empty() {
            return Err(Error::InvalidSpan("annotation name is empty"));
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        let mut line = serde_json::to_string(&annotation)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        inner.count += 1;
        inner
            .by_trace
            .entry(annotation.trace_id.clone())
            .or_default()
            .push(annotation);
        Ok(())
    }

    /// All annotations for a trace, optionally narrowed to one span or name.
    pub(crate) fn query(
        &self,
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
            .map(|entries| {
                entries
                    .iter()
                    .filter(|a| span_id.is_none() || span_id == Some(a.span_id.as_str()))
                    .filter(|a| name.is_none() || name == Some(a.name.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Drops annotations older than `cutoff_ns` by rewriting the log
    /// atomically (temp + rename). Returns how many were removed.
    pub(crate) fn drop_older_than(&self, cutoff_ns: u64) -> Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::LockPoisoned("annotations"))?;
        let mut kept: Vec<Annotation> = Vec::new();
        let mut removed = 0;
        for entries in inner.by_trace.values() {
            for annotation in entries {
                if annotation.timestamp_ns >= cutoff_ns {
                    kept.push(annotation.clone());
                } else {
                    removed += 1;
                }
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
        inner.by_trace.clear();
        inner.count = kept.len();
        for annotation in kept {
            inner
                .by_trace
                .entry(annotation.trace_id.clone())
                .or_default()
                .push(annotation);
        }
        Ok(removed)
    }
}
