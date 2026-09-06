//! Publication: a verified local pin becomes one immutable remote snapshot.
//!
//! The order is the contract, and the manifest is last:
//!
//! 1. **Validate the pin before opening anything it names.** The pin
//!    directory is canonicalized; its generation manifest is size-bounded
//!    and parsed; every manifested path must be a canonical pin file name
//!    (exact segment/payload/log naming, no duplicates, bounded counts);
//!    the pin's declared segment format must be the one this build reads;
//!    and the directory is walked to refuse symlinks, live-store artifacts
//!    (`wal.log`, `CURRENT`, `LOCK`, `generations/`, `pins/`, sidecars) and
//!    ANY unmanifested file — a stale manifest beside newer segments must
//!    refuse, never silently archive the older subset. Only then are the
//!    named files opened: length-checked, pending-erasure-checked (the
//!    pin's own tombstone log, bounded and strictly parsed), and finally
//!    digest-verified with the engine's own generation code.
//! 2. **Claim the name.** The id must carry no deletion tombstone and no
//!    manifest, and an upload-intent marker is CONDITIONALLY created; the
//!    second of two publishers loses here, and the marker is the durable
//!    state `cleanup` keys on.
//! 3. **Upload packs.** Files are concatenated into bounded pack objects
//!    (oversized files stream multipart as their own object, aborted on
//!    error). Each file's bytes are re-hashed *as they are read* and must
//!    still match the pin manifest; each object is then **fully read back
//!    from the remote and digest-verified** before it is believed durable.
//! 4. **Commit.** Immediately before the manifest is written the publisher
//!    re-checks that no deletion tombstone appeared and that its OWN intent
//!    marker still exists — a publisher resumed after a `cleanup` swept its
//!    objects aborts here instead of naming swept bytes. The manifest is
//!    then one conditional create; the tombstone is rechecked once more
//!    AFTER it lands (a delete or quiescent cleanup that retired the id
//!    while this create was in flight leaves the manifest as invisible
//!    debris, so reporting success would be a lie), and only then is the
//!    marker removed. This retires names permanently and lets a quiescent
//!    sweep run safely — it is not an HA or fencing guarantee.
//!
//! A crash or fault anywhere before the manifest leaves no visible
//! snapshot: readers require a manifest and no permanent tombstone.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::manifest::{
    classify_archive_path, object_name, valid_archive_path, ArchiveKind, RemoteFile,
    RemoteManifest, RemoteObject, STATE_MANIFEST_FILE,
};
use super::{Error, Remote, Result, CHUNK_BYTES, MAX_FILES, MAX_OBJECTS, PACK_TARGET_BYTES};

/// What one publication did.
#[derive(Clone, Debug)]
pub struct PublishReceipt {
    /// The snapshot id published.
    pub snapshot: String,
    /// The pinned generation archived.
    pub generation: u64,
    /// Files archived.
    pub files: usize,
    /// Pack objects written.
    pub objects: usize,
    /// Bytes uploaded, in full: pack objects, the intent marker, and the
    /// manifest.
    pub uploaded_bytes: u64,
    /// Bytes read back from the remote for post-upload verification —
    /// every pack object, in full, exactly once. (The manifest and marker
    /// are not read back; the manifest lands under a conditional create
    /// and is re-fetched by every reader.)
    pub verified_bytes: u64,
    /// SHA-256 (lowercase hex) of the manifest bytes as published. Retain
    /// it outside the bucket and hand it back through
    /// [`super::RemoteOptions::expected_manifest_sha256`] to detect a
    /// backend that rewrites history — the chunk tables authenticate data
    /// against the manifest, and this digest is what authenticates the
    /// manifest against you.
    pub manifest_sha256: String,
}

/// One file waiting to be packed.
struct PlanEntry {
    path: String,
    bytes: u64,
    sha256: String,
    local: PathBuf,
}

/// Refuses a pin directory that holds anything but exactly its manifested
/// files (plus its own `state-manifest.json`): symlinks anywhere, live-store
/// artifacts, sidecars, and — the stale-manifest case — load-bearing files
/// the manifest does not name. `root` must be canonical; the walk never
/// follows a link because it refuses every link it meets.
fn validate_pin_directory(root: &Path, manifested: &BTreeSet<String>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::Refused("pin walk escaped the pin directory".to_owned()))?
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(Error::Refused(format!(
                    "{relative:?} is a symlink; a pin holds only regular files"
                )));
            }
            if file_type.is_dir() {
                // Only the payload shards may nest; anything else — a
                // `generations/` or `pins/` tree, a directory shadowing a
                // file name — is not pin content.
                let legal = relative == "payloads"
                    || (relative.starts_with("payloads/")
                        && relative.split('/').count() == 2
                        && valid_archive_path(&relative));
                if !legal {
                    return Err(Error::Refused(format!(
                        "{relative:?} is a directory a pin never contains"
                    )));
                }
                pending.push(path);
                continue;
            }
            if relative == STATE_MANIFEST_FILE || manifested.contains(&relative) {
                continue;
            }
            // Name the refusal precisely: live-store artifacts mean "this
            // is not a pin", unmanifested load-bearing files mean "this pin
            // and its manifest disagree" — different operator responses.
            let live_artifact = matches!(relative.as_str(), "wal.log" | "CURRENT" | "LOCK")
                || relative.starts_with('.')
                || relative.ends_with(".rollup");
            if live_artifact {
                return Err(Error::Refused(format!(
                    "{relative:?} is a live-store artifact; publish from a pin \
                     (POST /v1/backups/<label> or `traza-object pin`), not a data directory"
                )));
            }
            return Err(Error::Refused(format!(
                "{relative:?} is not named by the pin's manifest — the manifest is stale or \
                 the directory is not a pin; refusing to archive a subset silently"
            )));
        }
    }
    Ok(())
}

pub(crate) fn publish_pin(
    remote: &Remote,
    pin_dir: &Path,
    snapshot_id: &str,
) -> Result<PublishReceipt> {
    if !super::valid_snapshot_id(snapshot_id) {
        return Err(Error::Refused(format!(
            "{snapshot_id:?} is not a valid snapshot id: [a-z0-9][a-z0-9._-]{{0,127}}, no `..`"
        )));
    }

    // ---- 1. validate the pin, in dependency order ------------------------
    // Canonical root first, so no later join can be steered through a
    // symlinked ancestor the walk below cannot see.
    let pin_dir = fs::canonicalize(pin_dir)?;

    // The generation manifest, size-bounded BEFORE it is read.
    let state_manifest_path = pin_dir.join(STATE_MANIFEST_FILE);
    let state_meta = fs::symlink_metadata(&state_manifest_path)?;
    if !state_meta.is_file() {
        return Err(Error::Refused(format!(
            "{STATE_MANIFEST_FILE} is not a regular file"
        )));
    }
    if state_meta.len() > super::MAX_MANIFEST_BYTES as u64 {
        return Err(Error::Refused(format!(
            "{STATE_MANIFEST_FILE} is {} bytes, over the {} bound",
            state_meta.len(),
            super::MAX_MANIFEST_BYTES
        )));
    }
    let pin_manifest = crate::generation::load_manifest(&state_manifest_path)?;

    // The pin's format must be one this build explicitly supports —
    // currently exactly the current segment format. An undeclared format is
    // a pre-v8 pin that needs migration, not archiving.
    match pin_manifest.segment_format {
        Some(format) if format == crate::segment::VERSION => {}
        Some(format) => {
            return Err(Error::Refused(format!(
                "pin declares segment format v{format}; this build archives v{} only",
                crate::segment::VERSION
            )))
        }
        None => {
            return Err(Error::Refused(
                "pin declares no segment format (pre-v8); migrate the store and re-pin \
                 before archiving"
                    .to_owned(),
            ))
        }
    }

    // Every manifested path must be canonical pin content, once.
    if pin_manifest.files.len() >= MAX_FILES {
        return Err(Error::Refused(format!(
            "{} files, over the {MAX_FILES} archive bound",
            pin_manifest.files.len()
        )));
    }
    let mut manifested: BTreeSet<String> = BTreeSet::new();
    for file in &pin_manifest.files {
        if !valid_archive_path(&file.path) {
            return Err(Error::Refused(format!(
                "manifested path {:?} is not a safe archive path",
                file.path
            )));
        }
        match classify_archive_path(&file.path) {
            Some(ArchiveKind::StateManifest) | None => {
                return Err(Error::Refused(format!(
                    "manifested path {:?} is not a canonical pin file name",
                    file.path
                )))
            }
            Some(_) => {}
        }
        if !manifested.insert(file.path.clone()) {
            return Err(Error::Refused(format!(
                "manifested path {:?} appears twice",
                file.path
            )));
        }
    }

    // The directory itself: nothing unmanifested, nothing linked, nothing
    // live.
    validate_pin_directory(&pin_dir, &manifested)?;

    // Now — and only now — the named files are opened. Lengths first.
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(pin_manifest.files.len() + 1);
    for file in &pin_manifest.files {
        let local = pin_dir.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let meta = fs::symlink_metadata(&local)?;
        if !meta.is_file() {
            return Err(Error::Refused(format!(
                "{:?} is not a regular file",
                file.path
            )));
        }
        if meta.len() != file.bytes {
            return Err(Error::Corrupt(format!(
                "{:?} is {} bytes on disk, {} in the pin manifest",
                file.path,
                meta.len(),
                file.bytes
            )));
        }
        entries.push(PlanEntry {
            path: file.path.clone(),
            bytes: file.bytes,
            sha256: file.sha256.clone(),
            local,
        });
    }
    // The generation manifest itself travels too, so a restore is
    // self-verifying and a reader can bind the remote manifest to it.
    entries.push(PlanEntry {
        path: STATE_MANIFEST_FILE.to_owned(),
        bytes: state_meta.len(),
        sha256: crate::payload::sha256_file(&state_manifest_path)?,
        local: state_manifest_path,
    });
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    // Pending erasures: the pin's own tombstone log, bounded and strictly
    // parsed. A pending record means the pin still holds the subject's
    // bytes, and an archive would serve them forever.
    let pending = crate::erasure::pending_count_in(&pin_dir)?;
    if pending > 0 {
        return Err(Error::Refused(format!(
            "the pin's tombstone log records {pending} pending erasure(s); its files still \
             hold the subject's bytes — settle the erasures and take a fresh pin"
        )));
    }

    // Digests, with the engine's own verification code.
    let problems = crate::generation::verify_against(&pin_dir, &pin_manifest)?;
    if !problems.is_empty() {
        return Err(Error::Refused(format!(
            "pin fails verification and will not be published: {}",
            problems.join("; ")
        )));
    }

    // ---- pack planning ---------------------------------------------------
    // Greedy in path order: whole files only, cut before the file that would
    // cross the target. A file larger than the target becomes its own
    // (multipart-streamed) object, so pack sizes are bounded by
    // max(PACK_TARGET_BYTES, largest single file).
    let mut packs: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_bytes: u64 = 0;
    for (index, entry) in entries.iter().enumerate() {
        if entry.bytes > PACK_TARGET_BYTES {
            if !current.is_empty() {
                packs.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            packs.push(vec![index]);
            continue;
        }
        if !current.is_empty() && current_bytes + entry.bytes > PACK_TARGET_BYTES {
            packs.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(index);
        current_bytes += entry.bytes;
    }
    if !current.is_empty() {
        packs.push(current);
    }
    if packs.len() > MAX_OBJECTS {
        return Err(Error::Refused(format!(
            "{} pack objects, over the {MAX_OBJECTS} archive bound",
            packs.len()
        )));
    }

    // ---- 2. claim the name ----------------------------------------------
    // A tombstoned id is dead forever; a manifested id is taken; an intent
    // marker means a publisher is (or was) at work.
    if remote
        .head_len(&remote.tombstone_path(snapshot_id))?
        .is_some()
    {
        return Err(Error::Refused(format!(
            "snapshot {snapshot_id:?} was deleted and its id is permanently retired; \
             snapshot ids are never reused — pick a new id"
        )));
    }
    if remote
        .head_len(&remote.manifest_path(snapshot_id))?
        .is_some()
    {
        return Err(Error::AlreadyExists(format!(
            "snapshot {snapshot_id:?} already exists; snapshots are immutable — \
             pick a new id (deleting this snapshot permanently retires its id)"
        )));
    }
    let intent = serde_json::json!({
        "snapshot": snapshot_id,
        "store": remote.store_identity(),
        "started_unix_ns": crate::unix_now_ns(),
    });
    let intent_bytes = serde_json::to_vec(&intent)
        .map_err(|error| Error::Corrupt(format!("intent does not encode: {error}")))?;
    let mut uploaded_bytes: u64 = intent_bytes.len() as u64;
    if !remote.put_create(&remote.intent_path(snapshot_id), intent_bytes)? {
        return Err(Error::AlreadyExists(format!(
            "snapshot {snapshot_id:?} has an upload in progress or abandoned; if you are \
             certain no publisher is running, `cleanup --publisher-quiescent` it and \
             publish under a new id — cleanup permanently retires the swept id"
        )));
    }

    // ---- 3. upload, verifying during and after --------------------------
    let mut files: Vec<RemoteFile> = Vec::with_capacity(entries.len());
    let mut objects: Vec<RemoteObject> = Vec::with_capacity(packs.len());
    let mut verified_bytes: u64 = 0;
    for pack in &packs {
        let object_index = objects.len();
        let name = object_name(object_index);
        let location = remote.object_path(snapshot_id, &name);
        let single_large = pack.len() == 1 && entries[pack[0]].bytes > PACK_TARGET_BYTES;
        let object = if single_large {
            let entry = &entries[pack[0]];
            let (sha256, chunks, sent) =
                remote.put_multipart_file(&location, &entry.local, entry.bytes)?;
            // A single-file object's digest IS the file's digest; the pin
            // manifest already vouched for the file, so a mismatch here is
            // a file that changed under us or a disk lying — refuse.
            if sha256 != entry.sha256 {
                return Err(Error::Corrupt(format!(
                    "{:?} hashed differently at upload than the pin manifest records",
                    entry.path
                )));
            }
            uploaded_bytes += sent;
            files.push(RemoteFile {
                path: entry.path.clone(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
                object: object_index as u32,
                offset: 0,
            });
            RemoteObject {
                name,
                bytes: entry.bytes,
                sha256,
                chunks,
            }
        } else {
            // In-memory pack: bounded by PACK_TARGET_BYTES by construction.
            let mut buffer: Vec<u8> = Vec::new();
            for &index in pack {
                let entry = &entries[index];
                let offset = buffer.len() as u64;
                let bytes = fs::read(&entry.local)?;
                if bytes.len() as u64 != entry.bytes {
                    return Err(Error::Corrupt(format!(
                        "{:?} changed length while being packed",
                        entry.path
                    )));
                }
                // Verified DURING upload, not just before: the hash the pack
                // carries is of the bytes actually read here.
                if crate::payload::sha256_hex(&bytes) != entry.sha256 {
                    return Err(Error::Corrupt(format!(
                        "{:?} hashed differently at packing than the pin manifest records",
                        entry.path
                    )));
                }
                buffer.extend_from_slice(&bytes);
                files.push(RemoteFile {
                    path: entry.path.clone(),
                    bytes: entry.bytes,
                    sha256: entry.sha256.clone(),
                    object: object_index as u32,
                    offset,
                });
            }
            let sha256 = crate::payload::sha256_hex(&buffer);
            let chunks: Vec<String> = buffer
                .chunks(CHUNK_BYTES as usize)
                .map(crate::payload::sha256_hex)
                .collect();
            let bytes = buffer.len() as u64;
            uploaded_bytes += bytes;
            remote.put(&location, buffer)?;
            RemoteObject {
                name,
                bytes,
                sha256,
                chunks,
            }
        };
        // Read back what the remote now claims to hold, in full, before the
        // manifest may name it. An ETag is not a checksum; this is.
        verified_bytes += remote.get_verify(&location, object.bytes, &object.sha256)?;
        objects.push(object);
    }

    // ---- 4. commit: manifest last, conditionally, fenced -----------------
    // Two checks immediately before the commit. The tombstone: a delete
    // that raced this upload has permanently retired the id, and a manifest
    // written now would be invisible debris at best. The marker: a cleanup
    // that swept this upload removed it, and committing would name objects
    // that no longer exist — a resumed publisher must abort, not publish.
    if remote
        .head_len(&remote.tombstone_path(snapshot_id))?
        .is_some()
    {
        return Err(Error::Refused(format!(
            "snapshot {snapshot_id:?} was deleted while this upload ran; its id is \
             permanently retired and this upload's objects are left for `cleanup`"
        )));
    }
    if remote.head_len(&remote.intent_path(snapshot_id))?.is_none() {
        return Err(Error::Refused(format!(
            "snapshot {snapshot_id:?}'s upload marker disappeared — a cleanup swept this \
             upload while it ran; nothing was published, and the swept id is now \
             permanently retired, so re-run the publish under a new id"
        )));
    }
    let manifest = RemoteManifest {
        format: super::manifest::REMOTE_FORMAT,
        snapshot: snapshot_id.to_owned(),
        store: remote.store_identity().to_owned(),
        created_unix_ns: crate::unix_now_ns(),
        source_generation: pin_manifest.generation,
        segment_format: crate::segment::VERSION,
        chunk_bytes: CHUNK_BYTES,
        files,
        objects,
    };
    let encoded = manifest.encode()?;
    let manifest_sha256 = crate::payload::sha256_hex(&encoded);
    uploaded_bytes += encoded.len() as u64;
    if !remote.put_create(&remote.manifest_path(snapshot_id), encoded)? {
        return Err(Error::AlreadyExists(format!(
            "snapshot {snapshot_id:?} was published concurrently by another writer; pick a \
             new id — this upload's objects remain under its prefix for `cleanup`"
        )));
    }
    // Cleanup may have retired the id while the manifest create was in
    // flight. In that case this manifest is invisible debris, and publish
    // must refuse success. Leave the tombstone intact and let a later
    // quiescent cleanup sweep any late writes.
    if remote
        .head_len(&remote.tombstone_path(snapshot_id))?
        .is_some()
    {
        return Err(Error::Refused(format!(
            "snapshot {snapshot_id:?} was deleted while this upload committed; its id is \
             permanently retired and this upload's manifest is left, invisible, for \
             `cleanup` — publish under a new id"
        )));
    }
    remote.delete(&remote.intent_path(snapshot_id))?;

    Ok(PublishReceipt {
        snapshot: snapshot_id.to_owned(),
        generation: pin_manifest.generation,
        files: manifest.files.len(),
        objects: manifest.objects.len(),
        uploaded_bytes,
        verified_bytes,
        manifest_sha256,
    })
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
            "traza-objpub-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn in_memory_remote() -> Remote {
        Remote::open(super::super::RemoteOptions::new(
            super::super::Backend::InMemory,
        ))
        .expect("remote")
    }

    /// A minimal, internally consistent pin directory: one segment, a
    /// tombstone log if asked, and a generation manifest digesting exactly
    /// what is present.
    fn minimal_pin(dir: &Path, with_pending_erasure: bool) {
        let segment = crate::segment::encode(&[crate::segment::RecordInput::new(
            1,
            "t1",
            std::collections::BTreeMap::new(),
            br#"{"trace_id":"t1","span_id":"s1","name":"s","service":"svc","start_time_ns":1,"end_time_ns":2}"#.to_vec(),
        )])
        .expect("segment");
        fs::write(dir.join("segment-00000000000000000001.seg"), &segment).expect("seg");
        if with_pending_erasure {
            let log = crate::erasure::ErasureLog::open(dir).expect("log");
            log.begin(
                1,
                crate::erasure::Subject::Trace {
                    trace_id: "t1".to_owned(),
                    tenant: String::new(),
                },
                vec![(String::new(), "t1".to_owned(), "s1".to_owned())],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("begin");
        }
        let manifest = crate::generation::Manifest {
            generation: 1,
            created_unix_ns: 0,
            folded_through: crate::generation::FoldedThrough::NONE,
            files: crate::generation::digest_engine(dir, &[]).expect("digest"),
            segment_format: Some(crate::segment::VERSION),
        };
        crate::generation::write_pin_manifest(dir, &manifest).expect("manifest");
    }

    #[test]
    fn a_pin_with_a_pending_erasure_is_refused_before_any_byte_leaves() {
        let dir = temp_dir("pending");
        minimal_pin(&dir, true);
        let remote = in_memory_remote();
        let error = publish_pin(&remote, &dir, "snap").expect_err("must refuse");
        let text = error.to_string();
        assert!(text.contains("pending"), "unexpected refusal: {text}");
        // Nothing was claimed or uploaded.
        assert!(remote
            .list_all(&remote.snapshot_root("snap"))
            .expect("list")
            .is_empty());
    }

    #[test]
    fn a_settled_erasure_does_not_block_publication_checks() {
        let dir = temp_dir("settled");
        let log = crate::erasure::ErasureLog::open(&dir).expect("log");
        let record = log
            .begin(
                1,
                crate::erasure::Subject::Trace {
                    trace_id: "t1".to_owned(),
                    tenant: String::new(),
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .expect("begin");
        log.record_settle(crate::erasure::SettleRecord {
            schema: crate::erasure::SettleRecord::schema_now(),
            id: record.id,
            settled_unix_ns: 2,
            generation: 1,
            spans_removed: 0,
            spans_redacted: 0,
            annotations_removed: 0,
            payloads_removed: Vec::new(),
            payloads_retained: Vec::new(),
            eval_records_removed: 0,
        })
        .expect("settle");
        assert_eq!(crate::erasure::pending_count_in(&dir).expect("count"), 0);
    }

    #[test]
    fn unmanifested_and_live_store_files_are_refused() {
        // A stale manifest beside a newer segment must refuse, not archive
        // the older subset.
        let dir = temp_dir("unmanifested");
        minimal_pin(&dir, false);
        fs::write(dir.join("segment-00000000000000000002.seg"), b"newer").expect("extra");
        let error = publish_pin(&in_memory_remote(), &dir, "snap").expect_err("must refuse");
        assert!(
            error
                .to_string()
                .contains("not named by the pin's manifest"),
            "{error}"
        );

        // A live-store artifact means "this is not a pin at all".
        let dir = temp_dir("live");
        minimal_pin(&dir, false);
        fs::write(dir.join("wal.log"), b"frames").expect("wal");
        let error = publish_pin(&in_memory_remote(), &dir, "snap").expect_err("must refuse");
        assert!(error.to_string().contains("live-store artifact"), "{error}");

        // A symlink is refused wherever it appears — the copy lives OUTSIDE
        // the pin so the link check is the only check that can fire.
        #[cfg(unix)]
        {
            let dir = temp_dir("symlink");
            let stash = temp_dir("symlink-stash");
            minimal_pin(&dir, false);
            let target = dir.join("segment-00000000000000000001.seg");
            let copy = stash.join("segment-bytes");
            fs::rename(&target, &copy).expect("stash");
            std::os::unix::fs::symlink(&copy, &target).expect("link");
            let error = publish_pin(&in_memory_remote(), &dir, "snap").expect_err("must refuse");
            assert!(error.to_string().contains("symlink"), "{error}");
        }
    }

    #[test]
    fn an_unsupported_pin_format_is_refused_before_upload() {
        let dir = temp_dir("format");
        minimal_pin(&dir, false);
        // Rewrite the manifest to claim an older format.
        let mut manifest =
            crate::generation::load_manifest(&dir.join(STATE_MANIFEST_FILE)).expect("manifest");
        manifest.segment_format = Some(7);
        crate::generation::write_pin_manifest(&dir, &manifest).expect("rewrite");
        let error = publish_pin(&in_memory_remote(), &dir, "snap").expect_err("must refuse");
        assert!(error.to_string().contains("v7"), "{error}");
    }

    #[test]
    fn pack_planning_bounds_pack_sizes() {
        // The arithmetic contract: many small files never build a pack past
        // the target, and the greedy cut keeps whole files.
        let sizes = [1_u64 << 20, 3 << 20, 3 << 20, 3 << 20, 9 << 20, 1 << 20];
        let mut packs: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut current_bytes = 0u64;
        for (index, &bytes) in sizes.iter().enumerate() {
            if bytes > PACK_TARGET_BYTES {
                if !current.is_empty() {
                    packs.push(std::mem::take(&mut current));
                    current_bytes = 0;
                }
                packs.push(vec![index]);
                continue;
            }
            if !current.is_empty() && current_bytes + bytes > PACK_TARGET_BYTES {
                packs.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            current.push(index);
            current_bytes += bytes;
        }
        if !current.is_empty() {
            packs.push(current);
        }
        for pack in &packs {
            let total: u64 = pack.iter().map(|&index| sizes[index]).sum();
            assert!(pack.len() == 1 || total <= PACK_TARGET_BYTES);
        }
        assert_eq!(packs.iter().map(Vec::len).sum::<usize>(), sizes.len());
    }
}
