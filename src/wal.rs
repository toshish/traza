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
//! JSON encoding of one batch of spans.
//!
//! **Recovery is strict about where the damage is.** A crash can only ever
//! tear the FINAL append, so a frame whose declared bytes are not all present
//! is dropped and the log is truncated back to the last good byte. A frame
//! that is structurally complete but fails its checksum or its decode is
//! damage in the MIDDLE of the log: stopping there would silently discard
//! every acknowledged batch after it, so it fails the open instead and says
//! where. Truncating the torn tail is what keeps that rule usable — garbage
//! left in place would sit mid-log after the next append and turn the next
//! restart into that hard failure.
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
//! Retention runs the other way: expiring a buffered span has to REWRITE the
//! log ([`Wal::rewrite`]), or the next restart replays exactly what TTL just
//! deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::{Error, Result, Span};

/// File name of the active log. One file: it is truncated at every flush, so
/// it never accumulates segments to rotate between.
const WAL_FILE_NAME: &str = "wal.log";

/// Staging name for an atomic rewrite. The leading dot and `.tmp` suffix are
/// what the orphan-temp sweep at open removes, so a crash before the rename
/// leaves nothing behind and the original log stays authoritative.
const WAL_REWRITE_TEMP: &str = ".wal.log.rewrite.tmp";

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

#[derive(Debug)]
struct WalState {
    /// The append handle. It lives inside the state because [`Wal::rewrite`]
    /// REPLACES the file: the rename orphans the old inode, so an append that
    /// kept using the old descriptor would write into a file nothing reads.
    file: File,
    /// Highest LSN whose bytes have reached the file descriptor.
    written_lsn: u64,
    /// Highest LSN known to be fsynced.
    durable_lsn: u64,
    /// A sync is in flight; new arrivals wait for it rather than piling on.
    syncing: bool,
    /// Bytes currently in the log. Maintained rather than stat'd because the
    /// flush policy consults it on every admitted batch.
    bytes: u64,
}

/// A batch encoded into its on-disk frame. See [`Wal::encode`].
#[derive(Debug)]
pub(crate) struct Frame(Vec<u8>);

impl Frame {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

/// The append-only log guarding acknowledged writes.
#[derive(Debug)]
pub(crate) struct Wal {
    directory: PathBuf,
    path: PathBuf,
    state: Mutex<WalState>,
    synced: Condvar,
    /// Deliberate delay before syncing, so more batches join the same fsync.
    /// See [`crate::Config::wal_commit_window`].
    commit_window: Option<std::time::Duration>,
}

impl Wal {
    /// Opens (creating if absent) the log in `directory`.
    ///
    /// Call [`Self::recover`] first: it is what decides how much of an
    /// existing log is trustworthy, and it truncates a torn tail so this
    /// handle appends after the last good byte.
    pub(crate) fn open(
        directory: &Path,
        commit_window: Option<std::time::Duration>,
    ) -> Result<Self> {
        let path = directory.join(WAL_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            directory: directory.to_path_buf(),
            path,
            state: Mutex::new(WalState {
                file,
                written_lsn: 0,
                durable_lsn: 0,
                syncing: false,
                bytes,
            }),
            synced: Condvar::new(),
            commit_window: commit_window.filter(|window| !window.is_zero()),
        })
    }

    /// Hands every span the log still holds to `sink`, oldest first, and
    /// truncates an interrupted final append.
    ///
    /// Replay is ORDER-PRESERVING and the caller upserts in that order, so the
    /// newest version of a re-ingested key wins exactly as it did before the
    /// crash. Frames are read one at a time rather than slurped: the log is
    /// bounded by the flush policy, but "bounded" is not "small", and a
    /// restart must not need the whole file plus its decoded spans resident at
    /// once.
    ///
    /// **What ends replay quietly, and what does not.** A frame missing bytes
    /// it declared can only be the append that the crash interrupted — it was
    /// never acknowledged, because the acknowledgement follows the fsync — so
    /// it is dropped and the file is truncated to the last complete frame.
    /// Anything else (a checksum mismatch, a payload that will not decode, a
    /// length field that cannot be real) is damage inside a frame that DID
    /// complete, and frames after it may be perfectly good acknowledged
    /// batches. Returning the prefix as if it were the whole log would lose
    /// them silently, so this fails instead.
    pub(crate) fn recover(directory: &Path, mut sink: impl FnMut(Span)) -> Result<()> {
        let path = directory.join(WAL_FILE_NAME);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        };
        let total = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut header = [0u8; HEADER_BYTES];
        let mut payload: Vec<u8> = Vec::new();
        // Bytes of complete, verified frames — the point the file is truncated
        // to if the tail turns out to be torn.
        let mut intact = 0u64;

        loop {
            let filled = read_fully(&mut reader, &mut header)?;
            if filled == 0 {
                break; // clean end of log
            }
            if filled < HEADER_BYTES {
                break; // torn tail: the header itself never completed
            }
            let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let checksum = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            if length == 0 || length > MAX_RECORD_BYTES {
                // A header that cannot describe a record is either corruption
                // or a zero-extended tail — some filesystems fill the gap of
                // an interrupted append with zeros rather than truncating it.
                // Only the all-zero case is a tail, and only if nothing but
                // zeros follows it.
                if header.iter().all(|byte| *byte == 0) && rest_is_all_zero(&mut reader)? {
                    break;
                }
                return Err(corrupt(
                    intact,
                    "declares a record length that cannot be real",
                ));
            }
            let remaining = total.saturating_sub(intact + HEADER_BYTES as u64);
            if u64::from(length) > remaining {
                break; // torn tail: the payload the header promised never landed
            }
            payload.resize(length as usize, 0);
            if read_fully(&mut reader, &mut payload)? < payload.len() {
                break; // torn tail, as above (short read against a full file)
            }
            if crc32(&payload) != checksum {
                return Err(corrupt(intact, "is complete but fails its checksum"));
            }
            let batch = serde_json::from_slice::<Vec<Span>>(&payload)
                .map_err(|error| corrupt(intact, &format!("does not decode: {error}")))?;
            for span in batch {
                sink(span);
            }
            intact += HEADER_BYTES as u64 + u64::from(length);
        }

        if intact < total {
            // Drop the torn tail NOW. Left in place it would be interior bytes
            // after the next append, and the strict rule above would then
            // refuse the following restart outright.
            let file = OpenOptions::new().write(true).open(&path)?;
            file.set_len(intact)?;
            file.sync_all()?;
        }
        Ok(())
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
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write-ahead log record exceeds u32 length",
            ))
        })?;
        if length > MAX_RECORD_BYTES {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
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
        state.file.write_all(&frame.0)?;
        state.bytes += frame.len();
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
            let mut target = state.written_lsn;
            // A private descriptor for the same file description, so the fsync
            // below needs no lock — that is the whole point of group commit.
            let mut handle = state.file.try_clone()?;
            drop(state);

            if let Some(window) = self.commit_window {
                // Hold the sync open for a moment so batches arriving now ride
                // along on it. The lock is NOT held across this sleep — the
                // whole point is that appends keep landing while we wait.
                std::thread::sleep(window);
                // Those late arrivals are already in the file, so the sync
                // about to run covers them too.
                let state = self.lock()?;
                target = state.written_lsn;
                handle = state.file.try_clone()?;
            }

            let result = metrics.wal_fsync.time(|| handle.sync_data());

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
        state.file.set_len(0)?;
        state.file.sync_data()?;
        state.bytes = 0;
        state.durable_lsn = state.written_lsn;
        self.synced.notify_all();
        Ok(())
    }

    /// Replaces the log with exactly `spans`, durably.
    ///
    /// This is the deletion path. Removing an expired span from the write
    /// buffer is not enough: the log still holds the frame that carried it, so
    /// the next restart replays it and the span comes back — TTL that a
    /// restart undoes is not retention, and for anyone deleting telemetry on
    /// request it is not deletion either. Rewriting drops those bytes from
    /// disk rather than marking them dead.
    ///
    /// Staged and renamed rather than truncated in place: the surviving spans
    /// are still acknowledged, and truncate-then-write would lose them to a
    /// crash in the middle. Callers hold the write-buffer lock, so no append
    /// can interleave; an in-flight commit is satisfied for the same reason
    /// [`Self::reset`] satisfies one — the content it was waiting on is
    /// durable in the file this publishes, or it was deliberately expired.
    pub(crate) fn rewrite(&self, spans: &[Span]) -> Result<()> {
        let frame = match spans.is_empty() {
            true => None,
            false => Some(Self::encode(spans)?),
        };
        let mut state = self.lock()?;
        let temp = self.directory.join(WAL_REWRITE_TEMP);
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            if let Some(frame) = &frame {
                file.write_all(&frame.0)?;
            }
            file.sync_all()?;
        }
        fs::rename(&temp, &self.path)?;
        crate::sync_directory(&self.directory)?;
        // The rename orphaned the inode the old descriptor names; append
        // through the published file from here on.
        state.file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        state.bytes = frame.map_or(0, |frame| frame.len());
        state.durable_lsn = state.written_lsn;
        self.synced.notify_all();
        Ok(())
    }

    /// Bytes the log currently holds — the work a restart would replay, and
    /// one of the bounds the flush policy enforces.
    pub(crate) fn size_bytes(&self) -> u64 {
        self.lock().map_or(0, |state| state.bytes)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, WalState>> {
        self.state.lock().map_err(|_| Error::LockPoisoned("wal"))
    }
}

/// Reads until `buffer` is full or the file ends, returning how much landed.
/// A short return therefore means EOF, never a partial read to retry.
fn read_fully(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// True when nothing but zero bytes remains. Read in chunks: this runs on the
/// recovery path, where the remainder is untrusted and may be large.
fn rest_is_all_zero(reader: &mut impl Read) -> io::Result<bool> {
    let mut chunk = [0u8; 8192];
    loop {
        let filled = read_fully(reader, &mut chunk)?;
        if chunk[..filled].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        if filled < chunk.len() {
            return Ok(true);
        }
    }
}

fn corrupt(offset: u64, problem: &str) -> Error {
    Error::WalCorrupt(format!(
        "the frame at byte {offset} of {WAL_FILE_NAME} {problem}. This is not an \
         interrupted final append, so frames after it may be acknowledged batches \
         that resuming would drop silently. Recover or move {WAL_FILE_NAME} aside \
         deliberately — moving it aside discards every acknowledged batch not yet \
         sealed into a segment"
    ))
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

    fn replayed(dir: &Path) -> Result<Vec<Span>> {
        let mut spans = Vec::new();
        Wal::recover(dir, |span| spans.push(span))?;
        Ok(spans)
    }

    fn append_committed(wal: &Wal, spans: &[Span]) {
        let lsn = wal
            .append(&Wal::encode(spans).expect("encode"))
            .expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");
    }

    #[test]
    fn replays_what_it_committed_in_order() {
        let dir = temp_dir("roundtrip");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "one"), span("t", "b", "two")]);
        append_committed(&wal, &[span("t", "c", "three")]);

        let replayed = replayed(&dir).expect("replay");
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
        assert!(replayed(&dir).expect("replay").is_empty());
    }

    #[test]
    fn a_torn_trailing_record_is_dropped_and_earlier_ones_kept() {
        let dir = temp_dir("torn");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "kept")]);
        append_committed(&wal, &[span("t", "b", "torn")]);

        // Chop the tail, as a crash mid-write would.
        let path = dir.join(WAL_FILE_NAME);
        let full = std::fs::metadata(&path).expect("meta").len();
        let file = OpenOptions::new().write(true).open(&path).expect("open");
        file.set_len(full - 4).expect("truncate");

        let replayed = replayed(&dir).expect("replay");
        assert_eq!(replayed.len(), 1, "only the intact record survives");
        assert_eq!(replayed[0].name, "kept");
    }

    #[test]
    fn a_torn_tail_is_truncated_so_later_appends_stay_replayable() {
        // Without truncation the torn bytes become INTERIOR bytes after the
        // next append, and the strict interior rule would refuse the restart
        // after that — a self-inflicted hard failure.
        let dir = temp_dir("torn-truncated");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "kept")]);
        append_committed(&wal, &[span("t", "b", "torn")]);
        drop(wal);

        let path = dir.join(WAL_FILE_NAME);
        let full = std::fs::metadata(&path).expect("meta").len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_len(full - 4)
            .expect("truncate");

        assert_eq!(replayed(&dir).expect("replay").len(), 1);
        let wal = Wal::open(&dir, None).expect("reopens");
        append_committed(&wal, &[span("t", "c", "after")]);

        let names: Vec<String> = replayed(&dir)
            .expect("replay")
            .into_iter()
            .map(|span| span.name)
            .collect();
        assert_eq!(names, ["kept", "after"], "the log is usable again");
    }

    #[test]
    fn a_complete_but_corrupt_frame_fails_recovery() {
        let dir = temp_dir("corrupt");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "kept")]);
        append_committed(&wal, &[span("t", "b", "rotten")]);

        // Flip a byte inside the SECOND record's payload. Every byte it
        // declared is present, so this is not a torn tail.
        let path = dir.join(WAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        let last = bytes.len() - 6;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");

        assert!(
            matches!(replayed(&dir), Err(Error::WalCorrupt(_))),
            "a complete corrupt frame is never quietly dropped"
        );
    }

    #[test]
    fn corruption_in_the_middle_does_not_discard_later_batches() {
        // The bug this guards: replay stopped at the first bad frame and
        // reported success, so acknowledged batches AFTER it vanished without
        // a word.
        let dir = temp_dir("middle");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "one")]);
        let after_first = std::fs::metadata(dir.join(WAL_FILE_NAME))
            .expect("meta")
            .len();
        append_committed(&wal, &[span("t", "b", "two")]);
        append_committed(&wal, &[span("t", "c", "three")]);

        // Corrupt the payload of the SECOND frame; the third stays valid.
        let path = dir.join(WAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        let target = after_first as usize + HEADER_BYTES + 2;
        bytes[target] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");

        match replayed(&dir) {
            Err(Error::WalCorrupt(message)) => {
                assert!(
                    message.contains(&after_first.to_string()),
                    "the error names the damaged offset: {message}"
                );
            }
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_extended_tail_is_treated_as_torn() {
        let dir = temp_dir("zero-tail");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "kept")]);
        drop(wal);

        // Some filesystems zero-fill the gap left by an interrupted append
        // instead of shortening the file.
        let path = dir.join(WAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes.extend_from_slice(&[0u8; 32]);
        std::fs::write(&path, &bytes).expect("write");

        let replayed = replayed(&dir).expect("a zero tail is not corruption");
        assert_eq!(replayed.len(), 1);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            bytes.len() as u64 - 32,
            "the zero tail is truncated away"
        );
    }

    #[test]
    fn reset_discards_the_log_and_releases_waiters() {
        let dir = temp_dir("reset");
        let wal = Wal::open(&dir, None).expect("opens");
        let lsn = wal
            .append(&Wal::encode(&[span("t", "a", "sealed")]).expect("encode"))
            .expect("append");
        wal.reset().expect("reset");
        assert!(replayed(&dir).expect("replay").is_empty());
        // The record is in a segment now, so its commit is already satisfied.
        wal.commit(lsn, &Metrics::default())
            .expect("commit after reset returns immediately");
        assert_eq!(wal.size_bytes(), 0);
    }

    #[test]
    fn rewrite_replaces_the_log_with_the_surviving_spans() {
        let dir = temp_dir("rewrite");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "expired"), span("t", "b", "kept")]);
        let before = wal.size_bytes();

        wal.rewrite(&[span("t", "b", "kept")]).expect("rewrite");
        assert!(wal.size_bytes() < before, "the expired bytes are gone");
        assert_eq!(
            wal.size_bytes(),
            std::fs::metadata(dir.join(WAL_FILE_NAME))
                .expect("meta")
                .len(),
            "the tracked size matches the file"
        );

        let names: Vec<String> = replayed(&dir)
            .expect("replay")
            .into_iter()
            .map(|span| span.name)
            .collect();
        assert_eq!(names, ["kept"]);

        // The reopened descriptor still appends to the published file.
        append_committed(&wal, &[span("t", "c", "later")]);
        let names: Vec<String> = replayed(&dir)
            .expect("replay")
            .into_iter()
            .map(|span| span.name)
            .collect();
        assert_eq!(names, ["kept", "later"]);
    }

    #[test]
    fn rewriting_to_nothing_empties_the_log() {
        let dir = temp_dir("rewrite-empty");
        let wal = Wal::open(&dir, None).expect("opens");
        append_committed(&wal, &[span("t", "a", "expired")]);
        wal.rewrite(&[]).expect("rewrite");
        assert_eq!(wal.size_bytes(), 0);
        assert!(replayed(&dir).expect("replay").is_empty());
    }

    #[test]
    fn concurrent_commits_are_covered_by_group_syncs() {
        let dir = temp_dir("group");
        let wal = Arc::new(Wal::open(&dir, None).expect("opens"));
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
        let replayed = replayed(&dir).expect("replay");
        assert_eq!(replayed.len(), 200, "every acknowledged record is durable");
    }
}
