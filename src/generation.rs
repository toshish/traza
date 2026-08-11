//! Generations: the one state every recovery domain agrees on.
//!
//! The engine's query-visible state spans several domains — the write-ahead
//! log and buffer, segments, `annotations.jsonl`, `payloads/` — and each has
//! its own durability rule and its own idea of "now". A **generation** names a
//! state they all agree on: a manifest listing every load-bearing engine file
//! with its digest, plus the log position (`folded_through`) at which the
//! manifest's contents end and replay begins. `CURRENT` names the live
//! generation, and moving it — one staged rename made durable by a directory
//! fsync — is the single commit point for a checkpoint, an install, or a
//! published deletion.
//!
//! **Layout.** The engine's files stay exactly where they were — a
//! generation *references* them rather than owning a copy. Segment paths are
//! load-bearing (recency order IS path order, expiry renames survivors onto
//! the same name, the supersede journal names paths), so nothing moves; the
//! manifest is what changes hands. The metadata a generation adds sits beside
//! the working set, in names the manifest's own whitelist never mistakes for
//! engine files:
//!
//! ```text
//! data/
//!   LOCK
//!   CURRENT                    -- decimal generation id; atomic-rename published
//!   wal.log                    -- v2 frames stamped (epoch, sequence)
//!   segment-*.seg              -- the working set: segments,
//!   annotations.jsonl          -- the annotation log,
//!   payloads/                  -- and offloaded payload bytes
//!   generations/<id>/state-manifest.json
//!   pins/<label>/              -- hard-link farm of one manifest's files
//! ```
//!
//! The manifest lists what recovery and verification are *about*: segments,
//! the annotation log, payload files. Rollup sidecars are derived caches
//! rebuilt on any failure, supersede journals are transient recovery state,
//! and the generation metadata is not itself engine state; none is listed,
//! and a digest walk skips the reserved subdirectories so a pinned segment is
//! never mistaken for a live one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{payload, Error, Result};

/// File naming the live generation. Its contents are one decimal id.
pub(crate) const CURRENT_NAME: &str = "CURRENT";
/// Staging name for publishing `CURRENT`; the orphan sweep removes strays.
const CURRENT_TEMP: &str = ".CURRENT.tmp";
/// Per-generation manifests live under here, one directory per id.
pub(crate) const GENERATIONS_DIR: &str = "generations";
/// Hard-link farms created by [`crate::Store::pin_generation`].
pub(crate) const PINS_DIR: &str = "pins";
/// The manifest file inside a generation (or pin, or staged install).
pub(crate) const MANIFEST_NAME: &str = "state-manifest.json";
/// Staging name for a manifest write.
const MANIFEST_TEMP: &str = ".state-manifest.json.tmp";

/// The log position a generation folded through: every frame at or before it
/// is inside the generation's files and must never replay; every frame
/// strictly after it is acknowledged work the generation does not yet hold.
///
/// Ordered as a tuple — epoch first, then sequence — which is what makes a
/// frame appended under a *newer* epoch (a checkpoint whose `CURRENT` rename
/// was not yet durable when the crash came) correctly replay against the
/// older generation the restart selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct FoldedThrough {
    /// The generation id frames were being stamped with.
    pub epoch: u64,
    /// The last stamped sequence the generation contains.
    pub sequence: u64,
}

impl FoldedThrough {
    /// The origin: nothing folded, every frame replays.
    pub(crate) const NONE: Self = Self {
        epoch: 0,
        sequence: 0,
    };
}

/// One load-bearing engine file, named relative to `engine/` with `/`
/// separators so a manifest written on one platform verifies on another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    /// Modification time, nanoseconds since the Unix epoch, or 0 where the
    /// platform would not say. Recorded ONLY so a later checkpoint can prove a
    /// file is unchanged and carry its digest over — never consulted by
    /// verification, which re-reads the bytes.
    #[serde(default)]
    pub modified_unix_ns: u64,
}

/// One immutable, self-describing, complete logical state of the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    /// Monotonically increasing generation id; `CURRENT` holds the live one.
    pub generation: u64,
    /// Wall-clock creation time, nanoseconds since the Unix epoch. Recorded
    /// for operators reading a backup's manifest; nothing orders by it.
    pub created_unix_ns: u64,
    /// See [`FoldedThrough`].
    pub folded_through: FoldedThrough,
    /// Every load-bearing file, digested.
    pub files: Vec<ManifestFile>,
}

/// Reads `CURRENT`, or `None` when the directory has never published one —
/// which is what distinguishes a pre-generation layout needing migration
/// from a directory this code has already adopted.
pub(crate) fn read_current(root: &Path) -> Result<Option<u64>> {
    match fs::read_to_string(root.join(CURRENT_NAME)) {
        Ok(contents) => {
            let id = contents.trim().parse::<u64>().map_err(|_| {
                Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{CURRENT_NAME} holds {contents:?}, which is not a generation id; \
                         refusing to guess which generation is live"
                    ),
                ))
            })?;
            Ok(Some(id))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

/// Publishes `CURRENT` naming `generation`: staged write, fsync, rename,
/// directory fsync. The directory fsync is the commit point — the rename is
/// visible immediately but crash-durable only once the directory entry is
/// synced, and nothing that depends on the new generation being live (log
/// reclamation above all) may act before this returns.
pub(crate) fn publish_current(root: &Path, generation: u64) -> Result<()> {
    let temp = root.join(CURRENT_TEMP);
    let staged = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        writeln!(file, "{generation}")?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, root.join(CURRENT_NAME))?;
    crate::sync_directory(root)
}

fn generation_dir(root: &Path, generation: u64) -> PathBuf {
    root.join(GENERATIONS_DIR).join(generation.to_string())
}

/// Where a generation's manifest lives.
pub(crate) fn manifest_path(root: &Path, generation: u64) -> PathBuf {
    generation_dir(root, generation).join(MANIFEST_NAME)
}

/// Loads and decodes one manifest.
pub(crate) fn load_manifest(path: &Path) -> Result<Manifest> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} does not decode as a manifest: {error}", path.display()),
        ))
    })
}

/// Writes `manifest` durably under its generation directory: staged, fsynced,
/// renamed, directory fsynced. Durable *before* `CURRENT` moves, or a restart
/// would point at a generation whose contents are not proven.
pub(crate) fn write_manifest(root: &Path, manifest: &Manifest) -> Result<()> {
    let dir = generation_dir(root, manifest.generation);
    fs::create_dir_all(&dir)?;
    let temp = dir.join(MANIFEST_TEMP);
    let staged = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, dir.join(MANIFEST_NAME))?;
    crate::sync_directory(&dir)
}

/// True for the files a manifest lists: segments, the annotation log, and
/// payload bytes. Everything else in the engine directory is derived or
/// transient.
fn is_manifested(relative: &str) -> bool {
    relative == "annotations.jsonl"
        || (relative.starts_with("segment-") && relative.ends_with(".seg"))
        || relative.starts_with("payloads/")
}

/// Walks the engine directory and digests every load-bearing file, reusing a
/// prior manifest's digest for any file whose path and length are unchanged.
///
/// The reuse is what keeps a checkpoint from costing a full corpus re-hash.
/// Segments are immutable once written (invariant 4: write-temp, fsync,
/// rename, and never touched again; expiry renames a *rewritten* file onto the
/// name, which changes its length), so a segment present in `prior` at the
/// same size is byte-identical and its digest carries over. Only segments
/// written since `prior` are hashed, plus the annotation log, whose
/// append-only growth changes its length and so re-hashes its (bounded)
/// current prefix. Pass an empty slice to force a full digest.
///
/// Runs with no engine lock held; callers serialize against anything that
/// replaces or removes files (the maintenance lock) and anything that adds
/// segments (the seal permit). Annotation appends may land mid-walk — the
/// annotation log is append-only, so the manifest records the length that was
/// digested and a verifier reads exactly that many bytes.
pub(crate) fn digest_engine(engine: &Path, prior: &[ManifestFile]) -> Result<Vec<ManifestFile>> {
    let mut carry: std::collections::HashMap<&str, &ManifestFile> =
        std::collections::HashMap::new();
    for file in prior {
        carry.insert(file.path.as_str(), file);
    }

    let mut files = Vec::new();
    let mut pending = vec![engine.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                // The metadata directories hold their own manifests and, in a
                // pin's case, hard links to segments — descending would digest
                // a pinned segment as if it were live. Only `payloads/` is a
                // manifested subtree.
                if name != GENERATIONS_DIR && name != PINS_DIR && !name.starts_with('.') {
                    pending.push(path);
                }
                continue;
            }
            let relative = path
                .strip_prefix(engine)
                .map_err(|_| {
                    Error::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "engine walk escaped the engine directory",
                    ))
                })?
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if !is_manifested(&relative) {
                continue;
            }
            let metadata = entry.metadata()?;
            let bytes = metadata.len();
            let modified_unix_ns = metadata
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
            // A file carries its digest over only when path, length AND
            // modification time all match. Length alone is nearly enough —
            // segments are immutable, payload files are content-addressed, and
            // expiry's in-place rewrite removes spans so it shrinks — but
            // "nearly" is the wrong standard for the number verification
            // trusts. Every file this engine publishes arrives by atomic
            // rename, so a rewritten one carries a new mtime.
            let unchanged = carry.get(relative.as_str()).filter(|prior| {
                prior.bytes == bytes
                    && prior.modified_unix_ns == modified_unix_ns
                    && prior.modified_unix_ns != 0
            });
            let sha256 = match unchanged {
                Some(prior) => prior.sha256.clone(),
                None => payload::sha256_file(&path)?,
            };
            files.push(ManifestFile {
                path: relative,
                bytes,
                sha256,
                modified_unix_ns,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// Re-reads `manifest` against the files under `engine`, checking existence,
/// length, and digest. Returns every discrepancy rather than the first, so an
/// operator sees the whole damage report in one pass.
///
/// The annotation log is verified over its manifested *prefix*: the file is
/// append-only, so bytes past the recorded length are appends since the
/// manifest and not damage.
pub(crate) fn verify_against(engine: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for file in &manifest.files {
        let path = engine.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                problems.push(format!("{}: missing", file.path));
                continue;
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let append_only = file.path == "annotations.jsonl";
        if metadata.len() != file.bytes && !(append_only && metadata.len() > file.bytes) {
            problems.push(format!(
                "{}: {} bytes on disk, {} in the manifest",
                file.path,
                metadata.len(),
                file.bytes
            ));
            continue;
        }
        let digest = if append_only && metadata.len() > file.bytes {
            sha256_prefix(&path, file.bytes)?
        } else {
            payload::sha256_file(&path)?
        };
        if digest != file.sha256 {
            problems.push(format!("{}: digest mismatch", file.path));
        }
    }
    Ok(problems)
}

/// SHA-256 over the first `length` bytes of a file — the append-only case,
/// where the manifested state is a prefix of the file on disk.
fn sha256_prefix(path: &Path, length: u64) -> Result<String> {
    use std::io::Read;
    let file = File::open(path)?;
    let mut remaining = length;
    let mut hasher = payload::Sha256::new();
    let mut reader = std::io::BufReader::new(file);
    let mut chunk = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = chunk.len().min(remaining as usize);
        let read = reader.read(&mut chunk[..want])?;
        if read == 0 {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} ended before its manifested length", path.display()),
            )));
        }
        hasher.update(&chunk[..read]);
        remaining -= read as u64;
    }
    Ok(hasher.finalize_hex())
}

/// Writes a manifest into a pin or staged-install directory (beside the
/// files it names, rather than under `generations/<id>/`), staged and fsynced.
pub(crate) fn write_pin_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let temp = dir.join(MANIFEST_TEMP);
    let staged = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, dir.join(MANIFEST_NAME))?;
    crate::sync_directory(dir)
}

/// Installs a staged generation as the live store: verify it, copy its files
/// into `root`, publish a fresh generation naming them. Restore is this; so is
/// a follower's snapshot install when HA arrives.
///
/// Runs offline, before any [`crate::Store`] opens `root`. The staged
/// directory holds a manifested engine file set and its manifest, exactly as
/// a pin does. The working set is laid down first (idempotent copies into a
/// directory the store does not yet consider live, since `CURRENT` still
/// names the prior generation or nothing), and the install commits at the
/// `CURRENT` rename: a crash before it leaves the prior store, a crash after
/// it leaves the installed one, never a blend.
///
/// `root` is expected to be empty or a prior store being replaced. A restored
/// store starts with an empty log — its state is entirely in the installed
/// files, and any prior log belonged to a different lineage.
pub(crate) fn install_staged(root: &Path, staged: &Path) -> Result<u64> {
    let manifest = load_manifest(&staged.join(MANIFEST_NAME))?;
    let problems = verify_against(staged, &manifest)?;
    if !problems.is_empty() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged generation fails verification and will not be installed: {}",
                problems.join("; ")
            ),
        )));
    }

    let next = read_current(root)?.unwrap_or(0).max(manifest.generation) + 1;

    // Remove any working-set file the prior store left that this generation
    // does not name, then copy each manifested file into place. Both are
    // idempotent: nothing here is live until CURRENT moves.
    remove_working_set(root)?;
    for file in &manifest.files {
        let relative = file.path.replace('/', std::path::MAIN_SEPARATOR_STR);
        let source = staged.join(&relative);
        let target = root.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &target)?;
    }
    let wal_path = root.join("wal.log");
    if wal_path.exists() {
        fs::remove_file(&wal_path)?;
    }
    crate::sync_directory(root)?;

    write_manifest(
        root,
        &Manifest {
            generation: next,
            created_unix_ns: manifest.created_unix_ns,
            folded_through: FoldedThrough::NONE,
            files: manifest.files.clone(),
        },
    )?;
    publish_current(root, next).map(|()| next)
}

/// Removes the manifested working set (segments, the annotation log, payload
/// files) from a directory, leaving metadata, the lock and the log. Used
/// before an install lays down a different generation's files.
fn remove_working_set(root: &Path) -> Result<()> {
    let mut pending = vec![(root.to_path_buf(), true)];
    while let Some((dir, top)) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                if top && name == "payloads" {
                    fs::remove_dir_all(entry.path())?;
                }
                continue;
            }
            if top && (name == "annotations.jsonl" || (name.starts_with("segment-"))) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Removes generation manifests older than `keep`, leaving the one `CURRENT`
/// names and anything a pin references. Manifests are small; this is
/// housekeeping, and a failure is reported by the caller as such rather than
/// failing the checkpoint that already committed.
pub(crate) fn sweep_generations(root: &Path, keep: u64) -> Result<()> {
    let dir = root.join(GENERATIONS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(id) = name.parse::<u64>() {
            if id < keep {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("traza-gen-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn current_round_trips_and_a_missing_current_is_none() {
        let root = temp_root("current");
        assert_eq!(read_current(&root).expect("read"), None);
        publish_current(&root, 7).expect("publish");
        assert_eq!(read_current(&root).expect("read"), Some(7));
        publish_current(&root, 8).expect("republish");
        assert_eq!(read_current(&root).expect("read"), Some(8));
    }

    #[test]
    fn a_current_that_does_not_parse_is_refused_not_guessed() {
        let root = temp_root("current-bad");
        fs::write(root.join(CURRENT_NAME), "generation-seven\n").expect("write");
        assert!(read_current(&root).is_err());
    }

    #[test]
    fn folded_through_orders_epoch_first() {
        let older = FoldedThrough {
            epoch: 3,
            sequence: 900,
        };
        let newer_epoch = FoldedThrough {
            epoch: 4,
            sequence: 1,
        };
        assert!(newer_epoch > older, "a later epoch outranks any sequence");
        assert!(FoldedThrough::NONE < older);
    }

    #[test]
    fn manifests_write_load_and_verify() {
        let root = temp_root("manifest");
        let engine = root.clone();
        fs::create_dir_all(engine.join("payloads/ab")).expect("dirs");
        fs::write(engine.join("segment-00000000000000000001.seg"), b"segment").expect("seg");
        fs::write(engine.join("annotations.jsonl"), b"{}\n").expect("ann");
        fs::write(engine.join("payloads/ab/abcd.bin"), b"payload").expect("pay");
        // Derived and transient files are not manifested.
        fs::write(engine.join("segment-00000000000000000001.seg.rollup"), b"x").expect("roll");
        fs::write(engine.join(".supersede.0002.journal"), b"x").expect("journal");

        let files = digest_engine(&engine, &[]).expect("digest");
        assert_eq!(
            files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            [
                "annotations.jsonl",
                "payloads/ab/abcd.bin",
                "segment-00000000000000000001.seg"
            ],
            "load-bearing files only, sorted"
        );

        let manifest = Manifest {
            generation: 1,
            created_unix_ns: 0,
            folded_through: FoldedThrough::NONE,
            files,
        };
        write_manifest(&root, &manifest).expect("write");
        let loaded = load_manifest(&manifest_path(&root, 1)).expect("load");
        assert_eq!(loaded.generation, 1);
        assert!(verify_against(&engine, &loaded).expect("verify").is_empty());

        // A flipped byte is caught by name.
        fs::write(engine.join("payloads/ab/abcd.bin"), b"tampered").expect("tamper");
        let problems = verify_against(&engine, &loaded).expect("verify");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("payloads/ab/abcd.bin"));
    }

    #[test]
    fn a_same_length_rewrite_does_not_carry_a_stale_digest() {
        // The reuse optimization's danger: a checkpoint carries a prior
        // digest over for an unchanged file, and "unchanged" judged by length
        // alone would hand verification a digest for bytes that are gone. The
        // length here is deliberately identical, which is the case length
        // alone cannot see.
        let root = temp_root("reuse");
        let engine = root.clone();
        let seg = engine.join("segment-00000000000000000001.seg");
        fs::write(&seg, b"aaaaaaaa").expect("write");
        let first = digest_engine(&engine, &[]).expect("digest");
        assert_eq!(first.len(), 1);

        // Rewrite in place, same byte count, different content.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&seg, b"bbbbbbbb").expect("rewrite");
        let second = digest_engine(&engine, &first).expect("digest");
        assert_eq!(second[0].bytes, first[0].bytes, "the length is unchanged");
        assert_ne!(
            second[0].sha256, first[0].sha256,
            "a rewritten file must be re-hashed, not carried over on length alone"
        );

        // And an genuinely untouched file DOES carry over, or the optimization
        // is not doing its job.
        let third = digest_engine(&engine, &second).expect("digest");
        assert_eq!(third[0].sha256, second[0].sha256);
        assert_eq!(
            third[0].modified_unix_ns, second[0].modified_unix_ns,
            "carried over on an unchanged path, length and mtime"
        );
    }

    #[test]
    fn annotation_appends_after_the_manifest_still_verify() {
        let root = temp_root("append");
        let engine = root.clone();
        fs::create_dir_all(&engine).expect("dirs");
        fs::write(engine.join("annotations.jsonl"), b"one\n").expect("ann");
        let manifest = Manifest {
            generation: 1,
            created_unix_ns: 0,
            folded_through: FoldedThrough::NONE,
            files: digest_engine(&engine, &[]).expect("digest"),
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(engine.join("annotations.jsonl"))
            .expect("open");
        file.write_all(b"two\n").expect("append");
        assert!(
            verify_against(&engine, &manifest)
                .expect("verify")
                .is_empty(),
            "bytes past the manifested prefix are appends, not damage"
        );
    }

    #[test]
    fn sweep_keeps_the_live_generation_and_later() {
        let root = temp_root("sweep");
        for id in 1..=3 {
            write_manifest(
                &root,
                &Manifest {
                    generation: id,
                    created_unix_ns: 0,
                    folded_through: FoldedThrough::NONE,
                    files: Vec::new(),
                },
            )
            .expect("write");
        }
        sweep_generations(&root, 3).expect("sweep");
        assert!(!manifest_path(&root, 1).exists());
        assert!(!manifest_path(&root, 2).exists());
        assert!(manifest_path(&root, 3).exists());
    }
}
