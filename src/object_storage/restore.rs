//! Full restore: a snapshot becomes a verified, pin-equivalent local
//! directory, committed by one atomic rename.
//!
//! Every byte is verified twice on the way down: chunk digests as the
//! object ranges stream in, and each assembled file against its own
//! manifest digest as it is written. The staged directory is then verified
//! a third way — against the embedded GENERATION manifest, with the same
//! code `Store::restore` runs — before the rename, so what lands under the
//! target name is exactly what `traza-server --restore` (or
//! [`crate::Store::restore`]) will accept, and a failure at any point
//! leaves only a staging directory to remove, never a half-target.
//!
//! No existing path is ever replaced: the target must not exist, files are
//! created `create_new`, and every path was validated against traversal
//! when the manifest was decoded. Nothing follows a symlink because nothing
//! pre-exists to be one.

use std::fs;
use std::io::Write;
use std::path::Path;

use super::manifest::STATE_MANIFEST_FILE;
use super::{Error, Remote, Result};

/// What one restore did.
#[derive(Clone, Debug)]
pub struct RestoreReceipt {
    /// The snapshot restored.
    pub snapshot: String,
    /// The generation the restored file set represents.
    pub generation: u64,
    /// Files materialized.
    pub files: usize,
    /// Bytes fetched from the remote.
    pub fetched_bytes: u64,
}

pub(crate) fn restore_snapshot(
    remote: &Remote,
    snapshot_id: &str,
    target: &Path,
) -> Result<RestoreReceipt> {
    // Destination precondition, stated exactly: the caller guarantees that
    // nothing else creates `target` while the restore runs. The target is
    // checked here, and re-checked immediately before the final rename, but
    // `std` offers no atomic no-replace directory rename — a path created
    // in the microseconds between the recheck and the rename can be
    // replaced if it is an empty directory. The rechecks bound the window;
    // the precondition is what closes it.
    if target.symlink_metadata().is_ok() {
        return Err(Error::Refused(format!(
            "{} already exists; restore only ever creates a fresh directory",
            target.display()
        )));
    }
    let parent = target
        .parent()
        .ok_or_else(|| Error::Refused(format!("{} has no parent directory", target.display())))?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Refused(format!("{} has no usable name", target.display())))?;
    fs::create_dir_all(parent)?;
    // Canonicalize the parent so no symlinked ancestor can steer where the
    // staging tree (and the final rename) actually land.
    let parent = fs::canonicalize(parent)?;
    let target = parent.join(name);

    // Open the snapshot the reader's way, so the fetch path (and its chunk
    // verification) is byte-for-byte the one queries exercise — including
    // the manifest gate and the archived generation cross-check.
    let snapshot = remote.open_snapshot(snapshot_id)?;
    let manifest = snapshot.manifest().clone();

    let staging = parent.join(format!(
        ".{name}.restoring-{}-{}",
        std::process::id(),
        crate::unix_now_ns()
    ));
    // Exclusive: reusing a pre-existing path (however unlikely the nonce
    // collision) could blend a previous attempt's files into this one.
    fs::create_dir(&staging)?;

    let built = (|| -> Result<u64> {
        let mut fetched: u64 = 0;
        // Every directory created under staging, for the bottom-up syncs
        // that make the tree durable before the rename publishes it.
        let mut created_dirs: std::collections::BTreeSet<std::path::PathBuf> =
            std::collections::BTreeSet::new();
        for (index, file) in manifest.files.iter().enumerate() {
            // Paths were validated at decode: relative, no `..`, safe
            // component charset, canonical pin names — joining under the
            // fresh, exclusively created staging root cannot escape it,
            // and nothing pre-exists in it to be a symlink.
            let local = staging.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(dir) = local.parent() {
                fs::create_dir_all(dir)?;
                let mut ancestor = dir.to_path_buf();
                while ancestor != staging {
                    created_dirs.insert(ancestor.clone());
                    match ancestor.parent() {
                        Some(next) => ancestor = next.to_path_buf(),
                        None => break,
                    }
                }
            }
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&local)?;
            // Stream the file's object range in chunk-table steps: each
            // fetched piece was digest-verified by the fetch layer, and the
            // running hash below verifies the ASSEMBLED file against its own
            // manifest digest — the object-level and file-level truths must
            // both hold.
            let step = u64::from(manifest.chunk_bytes);
            let mut hasher = crate::payload::Sha256::new();
            let mut offset: u64 = 0;
            while offset < file.bytes {
                let want = step.min(file.bytes - offset);
                let piece = snapshot_file_range(&snapshot, index, offset, want)?;
                hasher.update(&piece);
                out.write_all(&piece)?;
                offset += want;
                fetched += want;
            }
            if hasher.finalize_hex() != file.sha256 {
                return Err(Error::Corrupt(format!(
                    "{}: restored bytes do not match the manifest digest",
                    file.path
                )));
            }
            out.sync_all()?;
        }

        // Third verification, with the engine's own code: the staged set
        // must satisfy the generation manifest it carries, exactly as a
        // local pin must. This is what `Store::restore` will re-check.
        let generation_manifest =
            crate::generation::load_manifest(&staging.join(STATE_MANIFEST_FILE))?;
        let problems = crate::generation::verify_against(&staging, &generation_manifest)?;
        if !problems.is_empty() {
            return Err(Error::Corrupt(format!(
                "staged restore fails generation verification: {}",
                problems.join("; ")
            )));
        }
        // Directory entries bottom-up (deepest first — reverse
        // lexicographic order of full paths is deepest-first within one
        // tree), then the staging root, so every file's directory entry is
        // durable before the rename publishes the tree.
        for dir in created_dirs.iter().rev() {
            crate::sync_directory(dir)?;
        }
        crate::sync_directory(&staging)?;
        Ok(fetched)
    })();

    let fetched = match built {
        Ok(fetched) => fetched,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    // Recheck the destination as late as possible (see the precondition at
    // the top): a target that appeared during the download is refused, with
    // the staging removed rather than left behind.
    if target.symlink_metadata().is_ok() {
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::Refused(format!(
            "{} appeared while the restore ran; refusing to replace it",
            target.display()
        )));
    }
    if let Err(error) = fs::rename(&staging, &target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    crate::sync_directory(&parent)?;
    Ok(RestoreReceipt {
        snapshot: snapshot_id.to_owned(),
        generation: manifest.source_generation,
        files: manifest.files.len(),
        fetched_bytes: fetched,
    })
}

/// A range of one archived file through the snapshot's verified fetch path.
fn snapshot_file_range(
    snapshot: &super::RemoteSnapshot,
    file_index: usize,
    start: u64,
    len: u64,
) -> Result<Vec<u8>> {
    snapshot.fetch_file_range(file_index, start, len)
}
