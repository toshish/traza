//! Write-ahead log with group commit, stamped by generation.
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
//! **Record framing.** The file begins with the 8-byte magic `TRZWAL02`.
//! Each frame is `[u32 length][u32 crc32][u64 epoch][u64 sequence][payload]`,
//! the payload being the JSON encoding of one batch of spans. The epoch is
//! the generation id the frame was appended under and the sequence is
//! monotonic for the life of the store — a rewrite keeps counting rather than
//! restarting, which is what lets a manifest's `folded_through` mark a point
//! no later frame can slip behind.
//!
//! **Why frames carry their generation.** `CURRENT` and the log are separate
//! filesystem objects, so no rename can make "generation N+1 is live" and
//! "the frames folded into it are gone" one event. A crash between the two
//! would replay folded frames — including frames a published deletion
//! deliberately does not contain. The stamp closes it: recovery replays a
//! frame only when its `(epoch, sequence)` is strictly after the live
//! generation's `folded_through`, so reclaiming folded frames stops being
//! load-bearing for correctness and becomes housekeeping.
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
//! carries three dependencies. So on macOS a power cut can still lose an
//! acknowledged write. A kill -9, a panic, or an OS crash cannot, on either
//! platform, which is what tests/durability.rs proves.
//!
//! **Reclamation.** Once a flush seals the buffer into a segment, every WAL
//! record is superseded and the log is reset. Replaying a stale frame is
//! harmless even before the `folded_through` rule excludes it: records are
//! append-ordered, so upserting them in order reproduces the same
//! last-write-wins result the buffer already had. Retention runs the other
//! way: expiring a buffered span has to REWRITE the log ([`Wal::rewrite`]),
//! or the next restart replays exactly what TTL just deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::crc::crc32;
use crate::generation::FoldedThrough;
use crate::{Error, Result, Span};

/// File name of the active log, at the store root beside `CURRENT`.
const WAL_FILE_NAME: &str = "wal.log";

/// Staging name for an atomic rewrite. The leading dot and `.tmp` suffix are
/// what the orphan-temp sweep at open removes, so a crash before the rename
/// leaves nothing behind and the original log stays authoritative.
const WAL_REWRITE_TEMP: &str = ".wal.log.rewrite.tmp";

/// The magic that opens every v2 log. Its absence on a non-empty log is
/// corruption — the pre-generation framing is consumed once, at migration,
/// and never appended to again.
const WAL_MAGIC: &[u8; 8] = b"TRZWAL02";

/// Frame header: length and checksum (u32), then epoch and sequence (u64),
/// all little-endian.
const HEADER_BYTES: usize = 24;

/// The pre-generation header, kept only for [`Wal::recover_v1`]: length and
/// checksum, nothing else.
const V1_HEADER_BYTES: usize = 8;

/// A record larger than this is refused rather than trusted — a corrupt length
/// field must not make replay allocate gigabytes.
const MAX_RECORD_BYTES: u32 = 256 * 1024 * 1024;

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
    /// The generation id stamped onto new frames.
    epoch: u64,
    /// The last sequence stamped. Monotonic for the life of the store —
    /// carried across rewrites and epoch changes, never restarted, so no new
    /// frame can ever sort at or before a recorded `folded_through`.
    sequence: u64,
}

/// A batch encoded into its on-disk frame. The 16 stamp bytes are zero until
/// [`Wal::append`] fills them under its lock — encoding runs outside every
/// lock on purpose, and the stamp cannot be known until the append is ordered.
#[derive(Debug)]
pub(crate) struct Frame(Vec<u8>);

impl Frame {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn stamp(&mut self, epoch: u64, sequence: u64) {
        self.0[8..16].copy_from_slice(&epoch.to_le_bytes());
        self.0[16..24].copy_from_slice(&sequence.to_le_bytes());
    }
}

/// What [`Wal::recover`] learned: where the stamp counter must resume.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Recovered {
    /// The highest stamp seen anywhere in the log — including frames that
    /// were folded and therefore not replayed. The next stamp must be after
    /// BOTH this and the live manifest's `folded_through`.
    pub highest: FoldedThrough,
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
    /// Opens (creating if absent) the log in `directory`, stamping new frames
    /// with `epoch` and sequences after `resume_after`.
    ///
    /// Call [`Self::recover`] first: it is what decides how much of an
    /// existing log is trustworthy, truncates a torn tail so this handle
    /// appends after the last good byte, and reports the `resume_after` this
    /// open must honour.
    pub(crate) fn open(
        directory: &Path,
        commit_window: Option<std::time::Duration>,
        epoch: u64,
        resume_after: u64,
    ) -> Result<Self> {
        let path = directory.join(WAL_FILE_NAME);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let mut bytes = file.metadata()?.len();
        if bytes == 0 {
            file.write_all(WAL_MAGIC)?;
            file.sync_data()?;
            bytes = WAL_MAGIC.len() as u64;
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            path,
            state: Mutex::new(WalState {
                file,
                written_lsn: 0,
                durable_lsn: 0,
                syncing: false,
                bytes,
                epoch,
                sequence: resume_after,
            }),
            synced: Condvar::new(),
            commit_window: commit_window.filter(|window| !window.is_zero()),
        })
    }

    /// Hands every span the log holds STRICTLY AFTER `folded_through` to
    /// `sink`, oldest first, truncating an interrupted final append.
    ///
    /// Frames at or before `folded_through` are already inside the live
    /// generation's files and are discarded without decoding their spans —
    /// whether or not the roll-over that would have reclaimed them ever ran.
    /// This is the rule that makes a checkpoint's log reclamation
    /// housekeeping rather than correctness: a deletion the generation
    /// published cannot be replayed back into existence by a log that was
    /// never trimmed.
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
    /// length field that cannot be real, a missing magic) is damage inside a
    /// log that DID complete, and frames after it may be perfectly good
    /// acknowledged batches. Returning the prefix as if it were the whole log
    /// would lose them silently, so this fails instead.
    pub(crate) fn recover(
        directory: &Path,
        folded_through: FoldedThrough,
        mut sink: impl FnMut(Span),
    ) -> Result<Recovered> {
        let path = directory.join(WAL_FILE_NAME);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Recovered {
                    highest: FoldedThrough::NONE,
                })
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let total = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut highest = FoldedThrough::NONE;

        if total == 0 {
            return Ok(Recovered { highest });
        }
        let mut magic = [0u8; 8];
        let filled = read_fully(&mut reader, &mut magic)?;
        if filled < magic.len() || &magic != WAL_MAGIC {
            return Err(corrupt(
                0,
                "does not begin with the log magic. A log from before the \
                 generation layout is consumed by the migration at first open \
                 and never appended to afterwards, so a missing magic here is \
                 damage, not age",
            ));
        }

        let mut header = [0u8; HEADER_BYTES];
        let mut payload: Vec<u8> = Vec::new();
        // Bytes of complete, verified frames — the point the file is truncated
        // to if the tail turns out to be torn.
        let mut intact = WAL_MAGIC.len() as u64;

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
            let epoch = u64::from_le_bytes(header[8..16].try_into().expect("eight bytes"));
            let sequence = u64::from_le_bytes(header[16..24].try_into().expect("eight bytes"));
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
            let stamp = FoldedThrough { epoch, sequence };
            highest = highest.max(stamp);
            if stamp > folded_through {
                let batch = serde_json::from_slice::<Vec<Span>>(&payload)
                    .map_err(|error| corrupt(intact, &format!("does not decode: {error}")))?;
                for span in batch {
                    sink(span);
                }
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
        Ok(Recovered { highest })
    }

    /// Replays a pre-generation log — 8-byte headers, no magic, no stamps.
    ///
    /// Only the migration to the generation layout calls this, exactly once,
    /// before the store's first generation is published; the replayed spans
    /// land in the buffer and the migration rewrites the log in v2 framing.
    /// After migration a log without the magic is refused as damage.
    pub(crate) fn recover_v1(directory: &Path, mut sink: impl FnMut(Span)) -> Result<()> {
        let path = directory.join(WAL_FILE_NAME);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        };
        let total = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut header = [0u8; V1_HEADER_BYTES];
        let mut payload: Vec<u8> = Vec::new();
        let mut intact = 0u64;

        loop {
            let filled = read_fully(&mut reader, &mut header)?;
            if filled < V1_HEADER_BYTES {
                break;
            }
            let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let checksum = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            if length == 0 || length > MAX_RECORD_BYTES {
                if header.iter().all(|byte| *byte == 0) && rest_is_all_zero(&mut reader)? {
                    break;
                }
                return Err(corrupt(
                    intact,
                    "declares a record length that cannot be real",
                ));
            }
            let remaining = total.saturating_sub(intact + V1_HEADER_BYTES as u64);
            if u64::from(length) > remaining {
                break;
            }
            payload.resize(length as usize, 0);
            if read_fully(&mut reader, &mut payload)? < payload.len() {
                break;
            }
            if crc32(&payload) != checksum {
                return Err(corrupt(intact, "is complete but fails its checksum"));
            }
            let batch = serde_json::from_slice::<Vec<Span>>(&payload)
                .map_err(|error| corrupt(intact, &format!("does not decode: {error}")))?;
            for span in batch {
                sink(span);
            }
            intact += V1_HEADER_BYTES as u64 + u64::from(length);
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
        Self::encode_batch(spans)
    }

    /// [`Self::encode`] over any sequence that serializes as a span array —
    /// `&[Span]` on the ingest path, `&[&Span]` on the rewrite path, where
    /// copying the surviving half of the buffer to frame it would be waste.
    pub(crate) fn encode_batch<S: serde::Serialize>(spans: &[S]) -> Result<Frame> {
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
        bytes.extend_from_slice(&[0u8; 16]); // stamped at append, under the lock
        bytes.extend_from_slice(&payload);
        Ok(Frame(bytes))
    }

    /// Stamps and appends an encoded frame, returning its LSN. The bytes are
    /// in the file but NOT yet durable; the caller must [`Self::commit`]
    /// before acknowledging.
    pub(crate) fn append(
        &self,
        frame: &mut Frame,
        metrics: &crate::metrics::Metrics,
    ) -> Result<u64> {
        let waited = std::time::Instant::now();
        let mut state = self.lock()?;
        metrics
            .wal_lock_wait
            .record(waited.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        state.sequence += 1;
        let (epoch, sequence) = (state.epoch, state.sequence);
        frame.stamp(epoch, sequence);
        let writing = std::time::Instant::now();
        // The file is O_APPEND, so a single write_all lands contiguously at
        // the end even with other writers; the lock keeps LSNs ordered with
        // the bytes they name.
        state.file.write_all(&frame.0)?;
        metrics
            .wal_write_syscall
            .record(writing.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        state.bytes += frame.len();
        state.written_lsn += 1;
        Ok(state.written_lsn)
    }

    /// The last stamp handed out. Taken at a seal's drain — with the writer
    /// lock held, so no append can interleave — it is exactly the
    /// `folded_through` the resulting checkpoint may record: everything at or
    /// before it is in the drained spans, everything after arrived later.
    pub(crate) fn position(&self) -> Result<FoldedThrough> {
        let state = self.lock()?;
        Ok(FoldedThrough {
            epoch: state.epoch,
            sequence: state.sequence,
        })
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

    /// Discards the log's frames after a flush sealed its contents into a
    /// segment, leaving the magic in place for the appends that follow.
    ///
    /// Callers hold the write-buffer lock, so no append can interleave. Any
    /// commit still waiting is released: its spans are in a sealed segment,
    /// which is a stronger guarantee than the fsync it was waiting for. The
    /// stamp counter is NOT reset — sequences are monotonic for the life of
    /// the store, which is what keeps every future frame strictly after any
    /// recorded `folded_through`.
    pub(crate) fn reset(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.file.set_len(WAL_MAGIC.len() as u64)?;
        state.file.sync_data()?;
        state.bytes = WAL_MAGIC.len() as u64;
        state.durable_lsn = state.written_lsn;
        self.synced.notify_all();
        Ok(())
    }

    /// Moves the stamp to `epoch`. A checkpoint calls this AFTER its
    /// `CURRENT` rename is durable: frames appended from here on belong to
    /// the new generation, and the tuple order of [`FoldedThrough`] keeps
    /// them after every stamp of the old one.
    pub(crate) fn advance_epoch(&self, epoch: u64) -> Result<()> {
        let mut state = self.lock()?;
        state.epoch = state.epoch.max(epoch);
        Ok(())
    }

    /// Replaces the log with exactly `spans`, durably, stamped under `epoch`.
    ///
    /// This is the deletion path and the checkpoint's roll-over. Removing an
    /// expired span from the write buffer is not enough: the log still holds
    /// the frame that carried it, so the next restart replays it and the span
    /// comes back — TTL that a restart undoes is not retention, and for
    /// anyone deleting telemetry on request it is not deletion either.
    /// Rewriting drops those bytes from disk rather than marking them dead.
    ///
    /// Staged and renamed rather than truncated in place: the surviving spans
    /// are still acknowledged, and truncate-then-write would lose them to a
    /// crash in the middle. Callers hold the write-buffer lock, so no append
    /// can interleave; an in-flight commit is satisfied for the same reason
    /// [`Self::reset`] satisfies one — the content it was waiting on is
    /// durable in the file this publishes, or it was deliberately expired.
    pub(crate) fn rewrite<S: serde::Serialize>(&self, spans: &[S], epoch: u64) -> Result<()> {
        let mut frame = match spans.is_empty() {
            true => None,
            false => Some(Self::encode_batch(spans)?),
        };
        let mut state = self.lock()?;
        state.epoch = state.epoch.max(epoch);
        if let Some(frame) = &mut frame {
            state.sequence += 1;
            let (epoch, sequence) = (state.epoch, state.sequence);
            frame.stamp(epoch, sequence);
        }
        let bytes = WAL_MAGIC.len() as u64 + frame.as_ref().map_or(0, Frame::len);
        let temp = self.directory.join(WAL_REWRITE_TEMP);
        let staged = (|| -> Result<File> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            file.write_all(WAL_MAGIC)?;
            if let Some(frame) = &frame {
                file.write_all(&frame.0)?;
            }
            file.sync_all()?;
            // The append handle is opened on the STAGED inode, before the
            // rename carries that inode to the published name. Opening it
            // afterwards would put a fallible step after the point of no
            // return: if that open failed, the log on disk would already be
            // the new one while this `Wal` still pointed at the orphaned old
            // one, and the caller would have no way to tell which state it was
            // in. Everything that can fail now happens before the rename.
            Ok(OpenOptions::new().append(true).read(true).open(&temp)?)
        })();
        let published = match staged {
            Ok(published) => published,
            Err(error) => {
                // Nothing was replaced: the live log is untouched and the
                // caller may retry as if this had never run.
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
        };
        fs::rename(&temp, &self.path)?;

        // Past the rename the new log IS the log, so the in-memory state is
        // updated unconditionally — it can never disagree with the file — and
        // only then is the last remaining error reported. A crash before that
        // directory sync lands leaves the OLD log, which still holds the spans
        // the caller has not yet dropped from memory: recoverable, and in the
        // safe direction (a deletion is retried, never a deletion undone).
        state.file = published;
        state.bytes = bytes;
        state.durable_lsn = state.written_lsn;
        self.synced.notify_all();
        drop(state);
        crate::sync_directory(&self.directory)
    }

    /// Bytes the log currently holds — the work a restart would replay, and
    /// one of the bounds the flush policy enforces.
    ///
    /// The file's magic is excluded, so a reclaimed log measures zero rather
    /// than eight. That is not cosmetic: this number is both the
    /// `flush_wal_bytes` bound and the `wal_bytes` statistic, and both mean
    /// "replayable work". A constant preamble is neither replayed nor
    /// reclaimable, so counting it would make an empty log look like work and
    /// would put a permanent floor under a bound that exists to reach zero.
    pub(crate) fn size_bytes(&self) -> u64 {
        self.lock().map_or(0, |state| {
            state.bytes.saturating_sub(WAL_MAGIC.len() as u64)
        })
    }

    /// Writes a fresh v2 log holding `spans` as one frame stamped
    /// `(epoch, 1)`, replacing whatever file was there — the migration's
    /// conversion of a pre-generation log, staged and renamed so a crash
    /// leaves either the old log or the complete new one.
    pub(crate) fn write_fresh<S: serde::Serialize>(
        directory: &Path,
        spans: &[S],
        epoch: u64,
    ) -> Result<()> {
        let temp = directory.join(WAL_REWRITE_TEMP);
        let staged = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            file.write_all(WAL_MAGIC)?;
            if !spans.is_empty() {
                let mut frame = Self::encode_batch(spans)?;
                frame.stamp(epoch, 1);
                file.write_all(&frame.0)?;
            }
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = staged {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        fs::rename(&temp, directory.join(WAL_FILE_NAME))?;
        crate::sync_directory(directory)
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

    fn open(dir: &Path) -> Wal {
        Wal::open(dir, None, 1, 0).expect("opens")
    }

    fn replayed_after(dir: &Path, folded: FoldedThrough) -> Result<(Vec<Span>, Recovered)> {
        let mut spans = Vec::new();
        let recovered = Wal::recover(dir, folded, |span| spans.push(span))?;
        Ok((spans, recovered))
    }

    fn replayed(dir: &Path) -> Result<Vec<Span>> {
        replayed_after(dir, FoldedThrough::NONE).map(|(spans, _)| spans)
    }

    fn append_committed(wal: &Wal, spans: &[Span]) {
        let mut frame = Wal::encode(spans).expect("encode");
        let lsn = wal.append(&mut frame, &Metrics::default()).expect("append");
        wal.commit(lsn, &Metrics::default()).expect("commit");
    }

    #[test]
    fn replays_what_it_committed_in_order() {
        let dir = temp_dir("roundtrip");
        let wal = open(&dir);
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
    fn frames_at_or_before_folded_through_are_discarded() {
        let dir = temp_dir("folded");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "folded")]);
        append_committed(&wal, &[span("t", "b", "also-folded")]);
        let folded = wal.position().expect("position");
        append_committed(&wal, &[span("t", "c", "replayed")]);
        drop(wal);

        let (spans, recovered) = replayed_after(&dir, folded).expect("replay");
        assert_eq!(
            spans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["replayed"],
            "folded frames never replay, trimmed or not"
        );
        assert_eq!(
            recovered.highest,
            FoldedThrough {
                epoch: 1,
                sequence: 3
            },
            "the highest stamp counts folded frames too, so the counter resumes past them"
        );
    }

    #[test]
    fn a_newer_epoch_replays_against_an_older_generation() {
        // The crash-between-rename-and-fsync case: frames stamped under the
        // new generation exist while CURRENT rolled back to the old one.
        // Tuple order puts them strictly after the old folded_through, so
        // they replay — they are acknowledged spans the old generation does
        // not hold.
        let dir = temp_dir("epoch-rollback");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "old-epoch")]);
        let folded = wal.position().expect("position");
        wal.advance_epoch(2).expect("advance");
        append_committed(&wal, &[span("t", "b", "new-epoch")]);
        drop(wal);

        let (spans, _) = replayed_after(&dir, folded).expect("replay");
        assert_eq!(
            spans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["new-epoch"]
        );
    }

    #[test]
    fn a_torn_trailing_record_is_dropped_and_earlier_ones_kept() {
        let dir = temp_dir("torn");
        let wal = open(&dir);
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
        let wal = open(&dir);
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

        let (spans, recovered) = replayed_after(&dir, FoldedThrough::NONE).expect("replay");
        assert_eq!(spans.len(), 1);
        let wal = Wal::open(&dir, None, 1, recovered.highest.sequence).expect("reopens");
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
        let wal = open(&dir);
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
        let wal = open(&dir);
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
    fn a_missing_magic_on_a_nonempty_log_is_corruption() {
        let dir = temp_dir("no-magic");
        std::fs::write(dir.join(WAL_FILE_NAME), b"not a log at all").expect("write");
        assert!(matches!(replayed(&dir), Err(Error::WalCorrupt(_))));
    }

    #[test]
    fn the_v1_reader_replays_a_pre_generation_log() {
        // Hand-build an 8-byte-header frame, as the pre-generation engine
        // wrote them: the migration is the only caller of this path.
        let dir = temp_dir("v1");
        let payload = serde_json::to_vec(&[span("t", "a", "migrated")]).expect("payload");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        std::fs::write(dir.join(WAL_FILE_NAME), &bytes).expect("write");

        let mut spans = Vec::new();
        Wal::recover_v1(&dir, |span| spans.push(span)).expect("replay");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "migrated");
    }

    #[test]
    fn a_zero_extended_tail_is_treated_as_torn() {
        let dir = temp_dir("zero-tail");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "kept")]);
        drop(wal);

        // Some filesystems zero-fill the gap left by an interrupted append
        // instead of shortening the file.
        let path = dir.join(WAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes.extend_from_slice(&[0u8; 48]);
        std::fs::write(&path, &bytes).expect("write");

        let replayed = replayed(&dir).expect("a zero tail is not corruption");
        assert_eq!(replayed.len(), 1);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            bytes.len() as u64 - 48,
            "the zero tail is truncated away"
        );
    }

    #[test]
    fn reset_discards_frames_but_keeps_the_magic() {
        let dir = temp_dir("reset");
        let wal = open(&dir);
        let mut frame = Wal::encode(&[span("t", "a", "sealed")]).expect("encode");
        let lsn = wal.append(&mut frame, &Metrics::default()).expect("append");
        wal.reset().expect("reset");
        assert!(replayed(&dir).expect("replay").is_empty());
        // The record is in a segment now, so its commit is already satisfied.
        wal.commit(lsn, &Metrics::default())
            .expect("commit after reset returns immediately");
        assert_eq!(wal.size_bytes(), 0, "no replayable work is left");
        // The truncated log still opens as a v2 log.
        append_committed(&wal, &[span("t", "b", "after")]);
        assert_eq!(replayed(&dir).expect("replay").len(), 1);
    }

    #[test]
    fn rewrite_replaces_the_log_with_the_surviving_spans() {
        let dir = temp_dir("rewrite");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "expired"), span("t", "b", "kept")]);
        let before = wal.size_bytes();

        wal.rewrite(&[span("t", "b", "kept")], 1).expect("rewrite");
        assert!(wal.size_bytes() < before, "the expired bytes are gone");
        assert_eq!(
            wal.size_bytes() + WAL_MAGIC.len() as u64,
            std::fs::metadata(dir.join(WAL_FILE_NAME))
                .expect("meta")
                .len(),
            "the tracked size is the file minus its constant preamble"
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
    fn rewrite_sequences_stay_after_every_earlier_stamp() {
        // The counter never restarts: a rewritten frame must sort after any
        // folded_through recorded before it, or recovery would discard live
        // survivors as folded.
        let dir = temp_dir("rewrite-monotonic");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "one")]);
        append_committed(&wal, &[span("t", "b", "two")]);
        let folded = wal.position().expect("position");
        wal.rewrite(&[span("t", "b", "survivor")], 1)
            .expect("rewrite");
        drop(wal);

        let (spans, _) = replayed_after(&dir, folded).expect("replay");
        assert_eq!(
            spans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["survivor"],
            "the rewritten frame is stamped past the recorded position"
        );
    }

    #[test]
    fn rewriting_to_nothing_empties_the_log() {
        let dir = temp_dir("rewrite-empty");
        let wal = open(&dir);
        append_committed(&wal, &[span("t", "a", "expired")]);
        wal.rewrite::<Span>(&[], 1).expect("rewrite");
        assert_eq!(wal.size_bytes(), 0, "no replayable work is left");
        assert!(replayed(&dir).expect("replay").is_empty());
    }

    #[test]
    fn concurrent_commits_are_covered_by_group_syncs() {
        let dir = temp_dir("group");
        let wal = Arc::new(open(&dir));
        let mut handles = Vec::new();
        for worker in 0..8 {
            let wal = Arc::clone(&wal);
            handles.push(std::thread::spawn(move || {
                for index in 0..25 {
                    let id = format!("{worker}-{index}");
                    let mut frame = Wal::encode(&[span("t", &id, "x")]).expect("encode");
                    let lsn = wal.append(&mut frame, &Metrics::default()).expect("append");
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
