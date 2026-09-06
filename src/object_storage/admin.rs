//! Snapshot administration: list, inspect, verify, delete, cleanup.
//!
//! Deletion is fenced by a **permanent tombstone**: `delete` writes
//! `TOMBSTONE` under the snapshot's prefix FIRST and never removes it, so a
//! deleted id is dead forever — every manifest read refuses a tombstoned
//! id, and the publisher refuses to claim or commit one. That is what makes
//! deletion safe against a concurrent or resumed publisher without a
//! distributed lock: whoever's write lands second, the tombstone's presence
//! decides, and re-running an interrupted delete finishes the sweep. The
//! cost is stated in the operations guide: **snapshot ids are never
//! reusable after deletion.**
//!
//! Every path that reads a manifest, marker or tombstone checks the store
//! identity recorded in it against this remote's configured one (empty
//! configured identity skips the check), so no admin operation quietly
//! crosses store boundaries — deletion included, and the tombstone retains
//! the identity evidence after the manifest is gone.

use super::manifest::{RemoteManifest, STATE_MANIFEST_FILE};
use super::{Error, Remote, Result, MAX_MANIFEST_BYTES, MAX_RANGE_BYTES};

/// One snapshot as `list` sees it.
#[derive(Clone, Debug)]
pub struct SnapshotSummary {
    /// The snapshot id (its key component under the prefix).
    pub id: String,
    /// Whether a manifest exists — a complete, readable snapshot.
    pub complete: bool,
    /// Whether an upload-intent marker is present.
    pub uploading: bool,
    /// Whether a deletion tombstone is present: the id is permanently
    /// retired, whatever else remains under its prefix.
    pub deleted: bool,
    /// Whether the manifest records a store identity other than this
    /// remote's configured one. A foreign snapshot's details are withheld.
    pub foreign: bool,
    /// Publication time from the manifest, when complete and not foreign.
    pub created_unix_ns: Option<u64>,
    /// The archived generation, when complete and not foreign.
    pub source_generation: Option<u64>,
    /// Archived files, when complete and not foreign.
    pub files: Option<usize>,
    /// Total object bytes, when complete and not foreign.
    pub total_bytes: Option<u64>,
}

impl SnapshotSummary {
    fn bare(id: String, uploading: bool, deleted: bool) -> Self {
        Self {
            id,
            complete: false,
            uploading,
            deleted,
            foreign: false,
            created_unix_ns: None,
            source_generation: None,
            files: None,
            total_bytes: None,
        }
    }
}

/// What a verification pass found.
#[derive(Clone, Debug)]
pub struct VerifyReceipt {
    /// Objects checked.
    pub objects: usize,
    /// Bytes fully re-read and re-hashed (deep mode only).
    pub verified_bytes: u64,
    /// Every discrepancy, named. Empty means intact at the depth asked.
    pub problems: Vec<String>,
}

/// What a delete or cleanup removed.
#[derive(Clone, Debug)]
pub struct DeleteReceipt {
    /// Remote objects actually deleted (manifest and marker included; the
    /// permanent tombstone is never counted because it is never removed).
    /// Says nothing about unlisted debris — a crashed publisher's
    /// incomplete multipart parts are invisible to listing and need the
    /// bucket's lifecycle rule.
    pub objects_deleted: usize,
}

fn require_valid_id(snapshot_id: &str) -> Result<()> {
    if super::valid_snapshot_id(snapshot_id) {
        Ok(())
    } else {
        Err(Error::Refused(format!(
            "{snapshot_id:?} is not a valid snapshot id"
        )))
    }
}

/// Fetches, validates and identity-checks one snapshot's manifest — THE
/// gate every reader goes through. Tombstone first: a deleted id refuses
/// before a byte of manifest is fetched. When the remote carries an
/// expected manifest digest, the fetched bytes must hash to it.
pub(crate) fn fetch_manifest(remote: &Remote, snapshot_id: &str) -> Result<RemoteManifest> {
    fetch_manifest_with_digest(remote, snapshot_id).map(|(manifest, _)| manifest)
}

/// [`fetch_manifest`], also returning the SHA-256 of the manifest bytes as
/// fetched — the digest an operator retains externally.
pub(crate) fn fetch_manifest_with_digest(
    remote: &Remote,
    snapshot_id: &str,
) -> Result<(RemoteManifest, String)> {
    require_valid_id(snapshot_id)?;
    if let Some(tombstone) = remote.read_control(&remote.tombstone_path(snapshot_id))? {
        remote.check_identity(
            tombstone.get("store").and_then(|value| value.as_str()),
            "the deletion tombstone",
        )?;
        return Err(Error::NotFound(format!(
            "snapshot {snapshot_id:?} was deleted; its id is permanently retired"
        )));
    }
    let location = remote.manifest_path(snapshot_id);
    let bytes = match remote.get_all(&location, MAX_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(Error::NotFound(_)) => {
            return Err(Error::NotFound(format!(
                "snapshot {snapshot_id:?} has no manifest — it does not exist, or its \
                 publication never completed"
            )))
        }
        Err(error) => return Err(error),
    };
    let actual = crate::payload::sha256_hex(&bytes);
    if let Some(expected) = remote.expected_manifest_sha256() {
        if actual != expected {
            return Err(Error::Corrupt(format!(
                "snapshot {snapshot_id:?}: manifest hashes to {actual}, not the externally \
                 retained {expected} — the manifest was replaced or damaged"
            )));
        }
    }
    let manifest = RemoteManifest::decode(&bytes, snapshot_id)?;
    remote.check_identity(Some(manifest.store.as_str()), "this snapshot")?;
    Ok((manifest, actual))
}

/// One archived file, fetched whole by bounded ranged reads and verified
/// against its manifest digest and exact length. The no-cache path admin
/// operations use for the small control files (the generation manifest, the
/// tombstone log); [`super::RemoteSnapshot`] has its own chunk-verified
/// path.
pub(crate) fn fetch_archived_file(
    remote: &Remote,
    manifest: &RemoteManifest,
    snapshot_id: &str,
    path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let (_, file) = manifest
        .file(path)
        .ok_or_else(|| Error::Corrupt(format!("{path}: not in the snapshot's manifest")))?;
    if file.bytes > max_bytes {
        return Err(Error::Corrupt(format!(
            "{path}: {} bytes, over the {max_bytes} bound for this read",
            file.bytes
        )));
    }
    let object = &manifest.objects[file.object as usize];
    let location = remote.object_path(snapshot_id, &object.name);
    let mut collected = Vec::with_capacity(file.bytes as usize);
    let mut offset = file.offset;
    let end = file.offset + file.bytes;
    while offset < end {
        let step = (end - offset).min(MAX_RANGE_BYTES);
        collected.extend_from_slice(&remote.get_range(&location, offset..offset + step)?);
        offset += step;
    }
    if crate::payload::sha256_hex(&collected) != file.sha256 {
        return Err(Error::Corrupt(format!("{path}: digest mismatch")));
    }
    Ok(collected)
}

/// The archived-state validation every trusting reader runs: the remote
/// manifest must agree exactly with the generation manifest it archived,
/// and the archived tombstone log must strictly parse with zero PENDING
/// erasures — remote queries apply no erasure mask, so a pending record
/// would mean serving spans the source store was masking.
pub(crate) fn crosscheck_archived_state(
    remote: &Remote,
    manifest: &RemoteManifest,
    snapshot_id: &str,
) -> Result<()> {
    let generation_bytes = fetch_archived_file(
        remote,
        manifest,
        snapshot_id,
        STATE_MANIFEST_FILE,
        MAX_MANIFEST_BYTES as u64,
    )?;
    let generation = super::manifest::parse_generation_manifest(&generation_bytes)?;
    manifest.crosscheck_generation(&generation)?;
    if manifest.file("tombstones.jsonl").is_some() {
        let tombstones = fetch_archived_file(
            remote,
            manifest,
            snapshot_id,
            "tombstones.jsonl",
            crate::erasure::ARCHIVE_TOMBSTONE_LOG_BOUND,
        )?;
        let pending = crate::erasure::pending_count_in_bytes(&tombstones)
            .map_err(|error| Error::Corrupt(format!("archived tombstone log: {error}")))?;
        if pending > 0 {
            return Err(Error::Refused(format!(
                "the archived tombstone log records {pending} pending erasure(s); this \
                 snapshot should never have been published and will not be served"
            )));
        }
    }
    Ok(())
}

pub(crate) fn inspect(remote: &Remote, snapshot_id: &str) -> Result<RemoteManifest> {
    fetch_manifest(remote, snapshot_id)
}

pub(crate) fn list_snapshots(remote: &Remote) -> Result<Vec<SnapshotSummary>> {
    let mut ids = remote.list_child_dirs(&remote.snapshots_prefix())?;
    ids.sort();
    let mut summaries = Vec::with_capacity(ids.len());
    for id in ids {
        if !super::valid_snapshot_id(&id) {
            // A foreign key under our prefix: report it as debris rather
            // than guessing at it.
            summaries.push(SnapshotSummary::bare(id, false, false));
            continue;
        }
        let deleted = remote.head_len(&remote.tombstone_path(&id))?.is_some();
        let uploading = remote.head_len(&remote.intent_path(&id))?.is_some();
        let manifest_present = remote.head_len(&remote.manifest_path(&id))?.is_some();
        if deleted || !manifest_present {
            let mut summary = SnapshotSummary::bare(id, uploading, deleted);
            summary.complete = manifest_present && !deleted;
            summaries.push(summary);
            continue;
        }
        match fetch_manifest(remote, &id) {
            Ok(manifest) => summaries.push(SnapshotSummary {
                id,
                complete: true,
                uploading,
                deleted: false,
                foreign: false,
                created_unix_ns: Some(manifest.created_unix_ns),
                source_generation: Some(manifest.source_generation),
                files: Some(manifest.files.len()),
                total_bytes: Some(manifest.total_object_bytes()),
            }),
            Err(Error::Refused(_)) => {
                // Another store's snapshot under a shared prefix: listed,
                // named, contents withheld.
                let mut summary = SnapshotSummary::bare(id, uploading, false);
                summary.complete = true;
                summary.foreign = true;
                summaries.push(summary);
            }
            Err(_) => summaries.push(SnapshotSummary::bare(id, uploading, false)),
        }
    }
    Ok(summaries)
}

pub(crate) fn verify(remote: &Remote, snapshot_id: &str, deep: bool) -> Result<VerifyReceipt> {
    let manifest = fetch_manifest(remote, snapshot_id)?;
    let mut problems = Vec::new();
    // The archived-state binding is part of what "verified" means: a
    // manifest whose file set disagrees with the generation it archived, or
    // whose tombstone log hides a pending erasure, is a problem even when
    // every object's bytes are pristine.
    if let Err(error) = crosscheck_archived_state(remote, &manifest, snapshot_id) {
        problems.push(error.to_string());
    }
    let mut verified_bytes: u64 = 0;
    for object in &manifest.objects {
        let location = remote.object_path(snapshot_id, &object.name);
        match remote.head_len(&location)? {
            None => problems.push(format!("{}: missing", object.name)),
            Some(len) if len != object.bytes => problems.push(format!(
                "{}: {len} bytes on the remote, {} in the manifest",
                object.name, object.bytes
            )),
            Some(_) if deep => match remote.get_verify(&location, object.bytes, &object.sha256) {
                Ok(read) => verified_bytes += read,
                Err(Error::Corrupt(detail)) => problems.push(detail),
                Err(error) => return Err(error),
            },
            Some(_) => {}
        }
    }
    Ok(VerifyReceipt {
        objects: manifest.objects.len(),
        verified_bytes,
        problems,
    })
}

/// Sweeps every object under the snapshot's prefix except the permanent
/// tombstone, then proves the prefix is empty (tombstone aside). The shared
/// tail of `delete` and a tombstoned `cleanup`.
fn sweep_prefix(remote: &Remote, snapshot_id: &str) -> Result<usize> {
    let tombstone = remote.tombstone_path(snapshot_id);
    let root = remote.snapshot_root(snapshot_id);
    let mut deleted = 0usize;
    for (location, _) in remote.list_all(&root)? {
        if location == tombstone {
            continue;
        }
        remote.delete(&location)?;
        deleted += 1;
    }
    // Never report success over undeleted known objects: re-list and check.
    let remaining: Vec<_> = remote
        .list_all(&root)?
        .into_iter()
        .filter(|(location, _)| *location != tombstone)
        .collect();
    if !remaining.is_empty() {
        return Err(Error::Remote(format!(
            "{} object(s) remain under snapshot {snapshot_id:?} after deletion; re-run delete",
            remaining.len()
        )));
    }
    Ok(deleted)
}

/// Writes the permanent, identity-bound deletion tombstone for `snapshot_id`
/// and returns once it is durable — whether this call created it or a
/// concurrent delete already had. `store` is the SOURCE identity to record:
/// the manifest's own `store` for a published delete, the abandoned upload's
/// recorded store (or, failing that, this remote's configured identity) for
/// an abandoned-cleanup sweep. The create is CONDITIONAL, so a delete racing
/// this one cannot overwrite the ownership evidence; when this call loses
/// that race the already-present tombstone is read back and identity-checked
/// against this remote BEFORE the caller sweeps the shared prefix. The
/// tombstone is never removed: writing it retires the id forever, and it
/// survives the sweep so a resumed publisher can make nothing visible again.
fn retire_id(remote: &Remote, snapshot_id: &str, store: &str) -> Result<()> {
    let tombstone = serde_json::json!({
        "snapshot": snapshot_id,
        "store": store,
        "deleted_unix_ns": crate::unix_now_ns(),
    });
    let tombstone_bytes = serde_json::to_vec(&tombstone)
        .map_err(|error| Error::Corrupt(format!("tombstone does not encode: {error}")))?;
    if remote.put_create(&remote.tombstone_path(snapshot_id), tombstone_bytes)? {
        return Ok(());
    }
    // Lost the race: another delete retired this id first. Do not overwrite
    // its ownership evidence — read the existing tombstone back and confirm
    // this remote may act on it before the caller sweeps.
    let existing = remote
        .read_control(&remote.tombstone_path(snapshot_id))?
        .ok_or_else(|| {
            Error::Remote(format!(
                "snapshot {snapshot_id:?}: tombstone create lost the race yet no \
                 tombstone is readable; re-run delete"
            ))
        })?;
    remote.check_identity(
        existing.get("store").and_then(|value| value.as_str()),
        "the deletion tombstone",
    )
}

pub(crate) fn delete(remote: &Remote, snapshot_id: &str) -> Result<DeleteReceipt> {
    require_valid_id(snapshot_id)?;
    // Resume: a tombstone means a delete already committed; finish its
    // sweep. The tombstone carries the identity evidence the deleted
    // manifest no longer can.
    if let Some(tombstone) = remote.read_control(&remote.tombstone_path(snapshot_id))? {
        remote.check_identity(
            tombstone.get("store").and_then(|value| value.as_str()),
            "the deletion tombstone",
        )?;
        let deleted = sweep_prefix(remote, snapshot_id)?;
        return Ok(DeleteReceipt {
            objects_deleted: deleted,
        });
    }
    let manifest_location = remote.manifest_path(snapshot_id);
    let manifest_present = remote.head_len(&manifest_location)?.is_some();
    if !manifest_present {
        // No manifest, no tombstone: either nothing exists (idempotent
        // success) or an upload is in flight/abandoned, which deletion must
        // not race — that is cleanup's job, behind its explicit
        // quiescence acknowledgment.
        if remote.head_len(&remote.intent_path(snapshot_id))?.is_some() {
            return Err(Error::Refused(format!(
                "snapshot {snapshot_id:?} has an upload-intent marker and no manifest — a \
                 publication may be in flight; if you are certain no publisher is running, \
                 use cleanup with the publisher-quiescent acknowledgment"
            )));
        }
        return Ok(DeleteReceipt { objects_deleted: 0 });
    }
    // A published snapshot. Identity first — deletion must not cross store
    // boundaries — through the full manifest gate (tombstone known absent,
    // expected-digest check included).
    let manifest = fetch_manifest(remote, snapshot_id)?;
    // The fence, before any visibility change: once this lands, readers
    // refuse the id, publishers refuse to claim or commit it, and the id is
    // retired forever. The tombstone records the manifest's OWN source
    // identity, not this remote's configured one — an unconfigured client
    // (empty identity) may legitimately delete a foreign snapshot, and
    // stamping its empty identity would erase the ownership evidence the
    // tombstone must retain after the manifest is gone. The create is
    // conditional, so a delete racing this one cannot clobber that evidence.
    retire_id(remote, snapshot_id, &manifest.store)?;
    // Visibility next: after this no reader can open the snapshot even on
    // backends where the tombstone read races.
    remote.delete(&manifest_location)?;
    let deleted = 1 + sweep_prefix(remote, snapshot_id)?;
    Ok(DeleteReceipt {
        objects_deleted: deleted,
    })
}

pub(crate) fn cleanup(
    remote: &Remote,
    snapshot_id: &str,
    publisher_quiescent: bool,
) -> Result<DeleteReceipt> {
    require_valid_id(snapshot_id)?;
    // A tombstoned id: cleanup is a delete resume.
    if remote
        .head_len(&remote.tombstone_path(snapshot_id))?
        .is_some()
    {
        return delete(remote, snapshot_id);
    }
    let manifest_present = remote
        .head_len(&remote.manifest_path(snapshot_id))?
        .is_some();
    if manifest_present {
        // A complete snapshot: the only debris possible is a stale intent
        // marker (publisher crashed between manifest and marker removal).
        // Identity through the manifest gate before touching anything.
        let _manifest = fetch_manifest(remote, snapshot_id)?;
        let marker = remote.intent_path(snapshot_id);
        let had_marker = remote.head_len(&marker)?.is_some();
        if had_marker {
            remote.delete(&marker)?;
        }
        return Ok(DeleteReceipt {
            objects_deleted: usize::from(had_marker),
        });
    }
    // Abandoned (or in-flight!) publication. The sweep is safe ONLY when no
    // publisher is running: a live one could commit a manifest naming
    // objects this sweep removes. The caller must say so explicitly.
    if !publisher_quiescent {
        return Err(Error::Refused(format!(
            "snapshot {snapshot_id:?} has no manifest; sweeping its upload debris is safe \
             only when no publisher for this id is running anywhere — pass the \
             publisher-quiescent acknowledgment (CLI: --publisher-quiescent) to proceed"
        )));
    }
    // Retire the id permanently BEFORE sweeping a single object. The
    // tombstone survives the sweep, so a publisher for this id that resumes
    // afterwards can never turn its swept bytes into a visible snapshot, and
    // the swept id is never reused — a retry must pick a new id. Preserve the
    // source identity the abandoned upload recorded in its marker so an
    // unconfigured cleaner cannot erase it; with no marker to read, record
    // this remote's configured identity. (A configured cleaner still refuses
    // a marker that records a foreign, missing or malformed identity.)
    let source_store = match remote.read_control(&remote.intent_path(snapshot_id))? {
        Some(marker) => {
            let recorded = marker
                .get("store")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            remote.check_identity(recorded.as_deref(), "this upload's intent marker")?;
            recorded
        }
        None => None,
    };
    let store = source_store.unwrap_or_else(|| remote.store_identity().to_owned());
    retire_id(remote, snapshot_id, &store)?;
    let deleted = sweep_prefix(remote, snapshot_id)?;
    Ok(DeleteReceipt {
        objects_deleted: deleted,
    })
}
