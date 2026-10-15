//! `fuses.jsonl`: the durable half of the run guard.
//!
//! Live run state is a cache over the store and is rebuilt from spans; what
//! spans cannot reconstruct — policies, manual trips, resets and the grants
//! they carry — is written here, one JSON object per line, fsynced per append
//! and replayed at open. Most lines are superseded sooner or later (a policy
//! replaced, a kill switch reset), so once the log holds at least twice the
//! records that reproduce the state — and at least [`COMPACT_SLACK`] lines —
//! it is rewritten to just those: replay time and size track the state, not
//! its history. An erasure rewrites it too, without the erased subject. Both
//! rewrites replace the file atomically.
//!
//! It lives in the data directory OUTSIDE the generation layout: a backup
//! does not carry it and a restore does not replace it. Fuse settings are
//! operator configuration about the running service — like credentials —
//! rather than recorded telemetry, and a restore that silently reinstated last
//! month's limits, or lifted a kill switch pulled since, would be the more
//! surprising outcome.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::policy::{FusePolicy, Limits};
use crate::{Error, Result};

/// The log's file name in the data directory.
pub(crate) const LOG_NAME: &str = "fuses.jsonl";

/// Lines a log may hold before compaction is considered at all: below it,
/// a rewrite would save less than it costs.
pub(crate) const COMPACT_SLACK: usize = 1024;

/// One durable fuse fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Record {
    /// A policy created or replaced (same id) in full.
    Policy {
        /// The policy as it now stands.
        policy: FusePolicy,
    },
    /// A policy removed.
    DeletePolicy {
        /// The removed policy's id.
        id: String,
        /// When, Unix nanoseconds.
        at_ns: u64,
    },
    /// A manual stop: the kill switch.
    Trip {
        /// The run's tenant.
        #[serde(default)]
        tenant: String,
        /// The run id.
        run: String,
        /// The operator's reason, possibly empty.
        #[serde(default)]
        reason: String,
        /// When, Unix nanoseconds.
        at_ns: u64,
    },
    /// A stop cleared, optionally raising the run's limits.
    Reset {
        /// The run's tenant.
        #[serde(default)]
        tenant: String,
        /// The run id.
        run: String,
        /// Limits this reset raised, merged over earlier grants.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<Limits>,
        /// When, Unix nanoseconds.
        at_ns: u64,
    },
}

/// The open log: its path, for appends, and how many lines it holds.
#[derive(Debug)]
pub(crate) struct FuseLog {
    path: PathBuf,
    /// Records in the file.
    lines: usize,
    /// The line count at which compaction is next considered.
    compact_at: usize,
}

impl FuseLog {
    /// Opens the log in `directory`, returning it with every record it holds
    /// in append order.
    ///
    /// A torn final line — a crash mid-append — is truncated away before
    /// anything else is written, exactly as the annotation log heals one; a
    /// malformed line anywhere else is corruption and refuses the open,
    /// because skipping it would silently drop a later kill switch.
    pub(crate) fn open(directory: &Path) -> Result<(Self, Vec<Record>)> {
        let path = directory.join(LOG_NAME);
        // A rewrite a crash interrupted before its rename: the log itself is
        // intact, and the erasure that started the rewrite resumes and
        // redoes it.
        match fs::remove_file(path.with_extension("jsonl.tmp")) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let mut records = Vec::new();
        if path.exists() {
            let contents = fs::read(&path)?;
            let terminated = contents.ends_with(b"\n");
            let mut lines = contents.split_inclusive(|byte| *byte == b'\n').peekable();
            let mut valid_len = 0_u64;
            let mut healed = false;
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
                    Err(_) if lines.peek().is_none() && !terminated => {
                        let file = OpenOptions::new().write(true).open(&path)?;
                        file.set_len(valid_len)?;
                        file.sync_all()?;
                        healed = true;
                        break;
                    }
                    Err(error) => {
                        return Err(Error::Invalid(format!(
                            "{} is corrupt before its final line ({error}); refusing to open \
                             rather than drop the records after it",
                            path.display()
                        )))
                    }
                }
            }
            if !contents.is_empty() && !terminated && !healed {
                // A complete record whose newline was the torn byte: restore
                // the delimiter so the next append starts its own line.
                let mut file = OpenOptions::new().append(true).open(&path)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
        }
        let lines = records.len();
        Ok((
            Self {
                path,
                lines,
                // Considered at the first opportunity: a log that grew large
                // before this process started is compacted at open.
                compact_at: COMPACT_SLACK,
            },
            records,
        ))
    }

    /// Whether enough lines have accumulated to be worth comparing with the
    /// state; when it is, pass [`Self::compact`] the state's records.
    pub(crate) fn compaction_due(&self) -> bool {
        self.lines >= self.compact_at
    }

    /// Rewrites the log to `records` — the records that reproduce the state
    /// — when at least half its lines are superseded, and re-arms the check
    /// either way. Halving is what keeps compaction amortized O(1) per
    /// append. Returns whether it rewrote.
    pub(crate) fn compact(&mut self, records: &[Record]) -> Result<bool> {
        let rewrote = self.lines >= records.len().saturating_mul(2);
        if rewrote {
            self.rewrite(records)?;
        }
        self.compact_at = records
            .len()
            .saturating_mul(2)
            .max(self.lines.saturating_add(1))
            .max(COMPACT_SLACK);
        Ok(rewrote)
    }

    /// Replaces the whole log with `records`, atomically: written to a
    /// sibling, fsynced, renamed over the log, and the directory fsynced. A
    /// crash leaves the old log or the new one, never a blend — which is what
    /// an erasure needs, since a half-rewritten log could keep what it was
    /// asked to forget.
    pub(crate) fn rewrite(&mut self, records: &[Record]) -> Result<()> {
        let staged = self.path.with_extension("jsonl.tmp");
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record)?;
            bytes.push(b'\n');
        }
        let mut file = File::create(&staged)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&staged, &self.path)?;
        if let Some(directory) = self.path.parent() {
            crate::sync_directory(directory)?;
        }
        self.lines = records.len();
        self.compact_at = self.lines.saturating_mul(2).max(COMPACT_SLACK);
        Ok(())
    }

    /// Appends one record durably: written whole in one call, then fsynced.
    /// When this returns `Ok` the record survives a crash.
    pub(crate) fn append(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let fresh = !self.path.exists();
        let mut file: File = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.sync_all()?;
        if fresh {
            // The file's directory entry must be durable too, or the first
            // record can vanish with a crash that keeps its bytes.
            if let Some(directory) = self.path.parent() {
                crate::sync_directory(directory)?;
            }
        }
        self.lines = self.lines.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "traza-fuselog-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("dir");
        path
    }

    fn trip(run: &str) -> Record {
        Record::Trip {
            tenant: String::new(),
            run: run.into(),
            reason: "runaway".into(),
            at_ns: 7,
        }
    }

    #[test]
    fn records_round_trip_in_append_order() {
        let path = dir("roundtrip");
        let (mut log, records) = FuseLog::open(&path).expect("opens empty");
        assert!(records.is_empty());
        log.append(&trip("a")).expect("appends");
        log.append(&Record::Reset {
            tenant: "acme".into(),
            run: "a".into(),
            grant: Some(Limits {
                max_steps: Some(9),
                ..Limits::default()
            }),
            at_ns: 8,
        })
        .expect("appends");
        let (_, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], trip("a"));
        assert!(
            matches!(&records[1], Record::Reset { grant: Some(grant), .. }
            if grant.max_steps == Some(9))
        );
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_torn_tail_is_healed_and_inner_damage_refuses() {
        let path = dir("torn");
        let (mut log, _) = FuseLog::open(&path).expect("opens");
        log.append(&trip("a")).expect("appends");
        let file = path.join(LOG_NAME);
        let mut bytes = fs::read(&file).expect("reads");
        bytes.extend_from_slice(br#"{"op":"trip","run":"b","#);
        fs::write(&file, &bytes).expect("tears");

        let (mut log, records) = FuseLog::open(&path).expect("heals");
        assert_eq!(records, vec![trip("a")]);
        log.append(&trip("c")).expect("appends after healing");
        let (_, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records, vec![trip("a"), trip("c")]);

        // Damage before the final line is not a torn append.
        let mut bytes = b"not json\n".to_vec();
        bytes.extend_from_slice(&fs::read(&file).expect("reads"));
        fs::write(&file, &bytes).expect("corrupts");
        match FuseLog::open(&path) {
            Err(Error::Invalid(message)) => assert!(message.contains("corrupt"), "{message}"),
            other => panic!("inner damage must refuse the open, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_rewrite_replaces_the_log_whole() {
        let path = dir("rewrite");
        let (mut log, _) = FuseLog::open(&path).expect("opens");
        for run in ["a", "b", "c"] {
            log.append(&trip(run)).expect("appends");
        }
        log.rewrite(&[trip("b")]).expect("rewrites");
        let (mut log, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records, vec![trip("b")]);
        assert!(!path.join("fuses.jsonl.tmp").exists());
        log.append(&trip("d")).expect("appends after a rewrite");
        let (_, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records, vec![trip("b"), trip("d")]);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn compaction_rewrites_once_half_the_log_is_superseded() {
        let path = dir("compact");
        let (mut log, _) = FuseLog::open(&path).expect("opens");
        for _ in 0..COMPACT_SLACK - 1 {
            log.append(&trip("a")).expect("appends");
        }
        assert!(!log.compaction_due(), "below the slack nothing is checked");
        log.append(&trip("a")).expect("appends");
        assert!(log.compaction_due());
        // The state is one record: everything else is superseded.
        assert!(log.compact(&[trip("a")]).expect("compacts"));
        let (log, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records, vec![trip("a")]);
        assert!(!log.compaction_due());

        // A log that is mostly live — the state grew rather than churned —
        // is left alone, and the check re-arms past it instead of repeating
        // on every append.
        let state: Vec<Record> = (0..COMPACT_SLACK * 2)
            .map(|index| trip(&index.to_string()))
            .collect();
        let (mut log, _) = FuseLog::open(&path).expect("opens");
        log.rewrite(&state[..COMPACT_SLACK]).expect("rewrites");
        for record in &state[COMPACT_SLACK..] {
            log.append(record).expect("appends");
        }
        assert!(log.compaction_due());
        assert!(!log.compact(&state).expect("checks"));
        assert!(!log.compaction_due());
        let (_, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records.len(), COMPACT_SLACK * 2, "nothing was dropped");
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_record_missing_only_its_newline_is_kept_and_terminated() {
        let path = dir("newline");
        let file = path.join(LOG_NAME);
        let line = serde_json::to_vec(&trip("a")).expect("encodes");
        fs::write(&file, &line).expect("writes without newline");
        let (mut log, records) = FuseLog::open(&path).expect("opens");
        assert_eq!(records, vec![trip("a")]);
        log.append(&trip("b")).expect("appends");
        let (_, records) = FuseLog::open(&path).expect("reopens");
        assert_eq!(records, vec![trip("a"), trip("b")]);
        let _ = fs::remove_dir_all(&path);
    }
}
