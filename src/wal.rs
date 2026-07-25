//! Write-ahead log with group commit.
//!
//! The engine's segments are durable but sealed only at flush, so without a
//! log an acknowledged span lives in memory until then — a crash loses writes
//! the client was told succeeded. The WAL closes that gap: a batch is appended
//! and fsynced BEFORE ingest returns, and replayed into the write buffer on
//! open.
//!
//! **Group commit.** fsync, not the write, is the expensive part, so the sync
//! runs OUTSIDE the state lock: concurrent batches keep appending while one
//! thread is syncing, and a single fsync then covers all of them. A waiter
//! wakes when some sync has covered its LSN. This is what keeps per-batch cost
//! from becoming per-batch fsync under concurrency.
//!
//! **Record framing.** `[u32 length][u32 crc32][payload]`, payload being the
//! JSON encoding of one batch of spans. A crash can only ever tear the final
//! record, so replay stops at the first short or corrupt frame and keeps
//! everything before it. Trailing garbage is never interpreted.
//!
//! **What fsync buys, honestly.** `sync_data` is `fsync(2)`. On Linux that
//! carries the usual guarantee; on **macOS it does not flush the drive's own
//! write cache** — that needs `F_FULLFSYNC`, which std does not expose and
//! which this crate will not reach for while it forbids unsafe code and
//! carries two dependencies. So on macOS a power cut can still lose an
//! acknowledged write. A kill -9, a panic, or an OS crash cannot, on either
//! platform, which is what tests/durability.rs proves.
//!
//! **Reclamation.** Once a flush seals the buffer into a segment, every WAL
//! record is superseded and the log is truncated. Replaying a stale log is
//! harmless anyway: records are append-ordered, so upserting them in order
//! reproduces the same last-write-wins result the buffer already had.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::{Error, Result, Span};

/// File name of the active log. One file: it is truncated at every flush, so
/// it never accumulates segments to rotate between.
const WAL_FILE_NAME: &str = "wal.log";

/// Frame header: length and checksum, both little-endian u32.
const HEADER_BYTES: usize = 8;

/// A record larger than this is refused rather than trusted — a corrupt length
/// field must not make replay allocate gigabytes.
const MAX_RECORD_BYTES: u32 = 256 * 1024 * 1024;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[derive(Debug, Default)]
struct WalState {
    /// Highest LSN whose bytes have reached the file descriptor.
    written_lsn: u64,
    /// Highest LSN known to be fsynced.
    durable_lsn: u64,
    /// A sync is in flight; new arrivals wait for it rather than piling on.
    syncing: bool,
}

/// A batch encoded into its on-disk frame. See [`Wal::encode`].
#[derive(Debug)]
pub(crate) struct Frame(Vec<u8>);

/// The append-only log guarding acknowledged writes.
#[derive(Debug)]
pub(crate) struct Wal {
    file: File,
    path: PathBuf,
    state: Mutex<WalState>,
    synced: Condvar,
}

impl Wal {
    /// Opens (creating if absent) the log in `directory`.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(WAL_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            state: Mutex::new(WalState::default()),
            synced: Condvar::new(),
        })
    }

    /// Every span the log still holds, oldest first.
    ///
    /// Replay is ORDER-PRESERVING and the caller upserts in that order, so the
    /// newest version of a re-ingested key wins exactly as it did before the
    /// crash. A torn or corrupt trailing frame ends replay: it was never
    /// acknowledged, because the acknowledgement follows the fsync.
    pub(crate) fn replay(directory: &Path) -> Result<Vec<Span>> {
        let path = directory.join(WAL_FILE_NAME);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::Io(error)),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let mut spans = Vec::new();
        let mut offset = 0usize;
        while offset + HEADER_BYTES <= bytes.len() {
            let length = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            let checksum = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            if length == 0 || length > MAX_RECORD_BYTES {
                break;
            }
            let start = offset + HEADER_BYTES;
            let end = match start.checked_add(length as usize) {
                Some(end) if end <= bytes.len() => end,
                // Truncated tail: the record never completed its fsync.
                _ => break,
            };
            let payload = &bytes[start..end];
            if crc32(payload) != checksum {
                break;
            }
            match serde_json::from_slice::<Vec<Span>>(payload) {
                Ok(batch) => spans.extend(batch),
                // A frame that checksums but does not decode means the format
                // changed under us; stop rather than guess.
                Err(_) => break,
            }
            offset = end;
        }
        Ok(spans)
    }

    /// One batch serialized into its on-disk frame, ready to be appended.
    ///
    /// Encoding is separated from appending so the caller can do it BEFORE
    /// taking the engine's writer lock. Serializing a thousand-span batch is
    /// milliseconds of pure CPU; performing it under the lock stalled every
    /// other ingesting thread for that entire time, which capped concurrent
    /// ingest at roughly what one core could serialize.
    pub(crate) fn encode(spans: &[Span]) -> Result<Frame> {
        let payload = serde_json::to_vec(spans)?;
        let length = u32::try_from(payload.len()).map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write-ahead log record exceeds u32 length",
            ))
        })?;
        if length > MAX_RECORD_BYTES {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write-ahead log record exceeds the maximum record size",
            )));
        }
        let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        Ok(Frame(bytes))
    }

    /// Appends an encoded frame and returns its LSN. The bytes are in the file
    /// but NOT yet durable; the caller must [`Self::commit`] before
    /// acknowledging.
    pub(crate) fn append(&self, frame: &Frame) -> Result<u64> {
        let mut state = self.lock()?;
        // The file is O_APPEND, so a single write_all lands contiguously at
        // the end even with other writers; the lock keeps LSNs ordered with
        // the bytes they name.
        (&self.file).write_all(&frame.0)?;
        state.written_lsn += 1;
        Ok(state.written_lsn)
    }

    /// Blocks until `lsn` is fsynced. Safe to call from many threads: one
    /// syncs, the rest wait and are covered by that sync.
    pub(crate) fn commit(&self, lsn: u64, metrics: &crate::metrics::Metrics) -> Result<()> {
        metrics.wal_commits.increment();
        let mut state = self.lock()?;
        loop {
            if state.durable_lsn >= lsn {
                return Ok(());
            }
            if state.syncing {
                state = self
                    .synced
                    .wait(state)
                    .map_err(|_| Error::LockPoisoned("wal"))?;
                continue;
            }
            state.syncing = true;
            // Everything written BEFORE the sync starts is covered by it;
            // anything later is conservatively left for the next sync.
            let target = state.written_lsn;
            drop(state);

            let result = metrics.wal_fsync.time(|| self.file.sync_data());

            state = self.lock()?;
            state.syncing = false;
            if result.is_ok() {
                state.durable_lsn = state.durable_lsn.max(target);
            }
            self.synced.notify_all();
            result?;
        }
    }

    /// Discards the log after a flush sealed its contents into a segment.
    ///
    /// Callers hold the write-buffer lock, so no append can interleave. Any
    /// commit still waiting is released: its spans are in a sealed segment,
    /// which is a stronger guarantee than the fsync it was waiting for.
    pub(crate) fn reset(&self) -> Result<()> {
        let mut state = self.lock()?;
        self.file.set_len(0)?;
        self.file.sync_data()?;
        state.durable_lsn = state.written_lsn;
        self.synced.notify_all();
        Ok(())
    }

    /// Current size on disk, for diagnostics.
    pub(crate) fn size_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |meta| meta.len())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, WalState>> {
        self.state.lock().map_err(|_| Error::LockPoisoned("wal"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("traza-wal-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn span(trace: &str, id: &str, name: &str) -> Span {
        serde_json::from_value(serde_json::json!({
            "trace_id": trace, "span_id": id, "name": name, "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        }))
        .expect("span")
    }

    #[test]
    fn replays_what_it_committed_in_order() {
        let dir = temp_dir("roundtrip");
        let wal = Wal::open(&dir).expect("opens");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "a", "one"), span("t", "b", "two")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "c", "three")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");

        let replayed = Wal::replay(&dir).expect("replay");
        assert_eq!(replayed.len(), 3);
        assert_eq!(
            replayed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["one", "two", "three"],
            "append order is preserved, which is what makes replay last-write-wins"
        );
    }

    #[test]
    fn replay_of_an_absent_log_is_empty() {
        let dir = temp_dir("absent");
        assert!(Wal::replay(&dir).expect("replay").is_empty());
    }

    #[test]
    fn a_torn_trailing_record_is_dropped_and_earlier_ones_kept() {
        let dir = temp_dir("torn");
        let wal = Wal::open(&dir).expect("opens");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "a", "kept")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "b", "torn")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");

        // Chop the tail, as a crash mid-write would.
        let path = dir.join(WAL_FILE_NAME);
        let full = std::fs::metadata(&path).expect("meta").len();
        let file = OpenOptions::new().write(true).open(&path).expect("open");
        file.set_len(full - 4).expect("truncate");

        let replayed = Wal::replay(&dir).expect("replay");
        assert_eq!(replayed.len(), 1, "only the intact record survives");
        assert_eq!(replayed[0].name, "kept");
    }

    #[test]
    fn a_corrupt_payload_ends_replay_without_being_trusted() {
        let dir = temp_dir("corrupt");
        let wal = Wal::open(&dir).expect("opens");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "a", "kept")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "b", "rotten")]).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");

        // Flip a byte inside the SECOND record's payload.
        let path = dir.join(WAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        let last = bytes.len() - 6;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");

        let replayed = Wal::replay(&dir).expect("replay");
        assert_eq!(replayed.len(), 1, "checksum mismatch is not replayed");
        assert_eq!(replayed[0].name, "kept");
    }

    #[test]
    fn reset_discards_the_log_and_releases_waiters() {
        let dir = temp_dir("reset");
        let wal = Wal::open(&dir).expect("opens");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "a", "sealed")]).expect("encode"))
            .expect("append");
        wal.reset().expect("reset");
        assert!(Wal::replay(&dir).expect("replay").is_empty());
        // The record is in a segment now, so its commit is already satisfied.
        wal.commit(lsn, &Metrics::default())
            .expect("commit after reset returns immediately");
        assert_eq!(wal.size_bytes(), 0);
    }

    #[test]
    fn concurrent_commits_are_covered_by_group_syncs() {
        let dir = temp_dir("group");
        let wal = Arc::new(Wal::open(&dir).expect("opens"));
        let mut handles = Vec::new();
        for worker in 0..8 {
            let wal = Arc::clone(&wal);
            handles.push(std::thread::spawn(move || {
                for index in 0..25 {
                    let id = format!("{worker}-{index}");
                    let lsn = wal
                        .append(&Wal::encode(&[span("t", &id, "x")]).expect("encode"))
                        .expect("append");
                    wal.commit(lsn, &Metrics::default()).expect("commit");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker");
        }
        let replayed = Wal::replay(&dir).expect("replay");
        assert_eq!(replayed.len(), 200, "every acknowledged record is durable");
    }
}
