//! The remote snapshot manifest: what one snapshot is, stated completely.
//!
//! A manifest names every archived file, the pack object and offset holding
//! its bytes, and per-object chunk digests — everything a reader needs to
//! range-read, verify, or restore the snapshot without listing the bucket or
//! trusting it. It is written LAST during publication (conditionally, so a
//! name has exactly one winner) and is the visibility switch: no manifest,
//! no snapshot.
//!
//! Decoding is hostile-input parsing. Every path is validated against
//! traversal and duplication, every count against its bound, every geometry
//! against the arithmetic that will later trust it — a malformed manifest is
//! refused whole, never partially believed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::{Error, Result, MAX_FILES, MAX_OBJECTS};

/// The remote manifest format generation this build writes and reads.
pub(crate) const REMOTE_FORMAT: u32 = 1;
/// The archived copy of the pin's generation manifest, restored verbatim so
/// `Store::restore` can verify the staged directory exactly as it verifies
/// a local pin.
pub(crate) const STATE_MANIFEST_FILE: &str = "state-manifest.json";

/// What one archived path IS, under the pin format's exact naming. Anything
/// that does not classify is not a load-bearing pin file and is refused —
/// both in a purported source pin and in a fetched remote manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    /// `segment-{20 decimal digits}.seg`, digits parsing as a `u64`.
    Segment,
    /// `payloads/{2 lowercase hex}/{64 lowercase hex}.bin`, shard equal to
    /// the hash's first two characters.
    Payload,
    /// `annotations.jsonl`.
    AnnotationLog,
    /// `evals.jsonl`.
    EvalLog,
    /// `tombstones.jsonl`.
    TombstoneLog,
    /// `state-manifest.json` — legal only as the outer archive's own entry,
    /// never inside a generation manifest.
    StateManifest,
}

/// Classifies `path` under the pin format's exact canonical naming, or
/// `None` for anything else. This is the allowlist: safe path SYNTAX is not
/// enough, because a syntactically safe foreign name (`extra.bin`, a
/// mis-sharded payload, a segment name that is not a u64) is either debris
/// or a forgery, and both are refused.
pub(crate) fn classify_archive_path(path: &str) -> Option<ArchiveKind> {
    match path {
        "annotations.jsonl" => return Some(ArchiveKind::AnnotationLog),
        "evals.jsonl" => return Some(ArchiveKind::EvalLog),
        "tombstones.jsonl" => return Some(ArchiveKind::TombstoneLog),
        STATE_MANIFEST_FILE => return Some(ArchiveKind::StateManifest),
        _ => {}
    }
    if let Some(digits) = path
        .strip_prefix("segment-")
        .and_then(|rest| rest.strip_suffix(".seg"))
    {
        let canonical = digits.len() == 20
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && digits.parse::<u64>().is_ok();
        return canonical.then_some(ArchiveKind::Segment);
    }
    if let Some(rest) = path.strip_prefix("payloads/") {
        let mut parts = rest.split('/');
        let (Some(shard), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        let hash = file.strip_suffix(".bin")?;
        let lower_hex = |text: &str| {
            text.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let canonical = shard.len() == 2
            && hash.len() == 64
            && lower_hex(shard)
            && lower_hex(hash)
            && hash.starts_with(shard);
        return canonical.then_some(ArchiveKind::Payload);
    }
    None
}

/// One archived file: the pin-relative path queries and restore address it
/// by, its own digest, and where its bytes sit inside a pack object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteFile {
    /// Pin-relative path with `/` separators (`segment-…​.seg`,
    /// `payloads/ab/…​.bin`, `annotations.jsonl`, `state-manifest.json`).
    pub path: String,
    /// Exact byte length.
    pub bytes: u64,
    /// SHA-256 of the file's bytes, lowercase hex.
    pub sha256: String,
    /// Index into [`RemoteManifest::objects`] of the pack holding the bytes.
    pub object: u32,
    /// Byte offset of the file inside that pack.
    pub offset: u64,
}

/// One snapshot-owned pack object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteObject {
    /// Object name relative to the snapshot root (`objects/NNNNNNNN.pack`).
    pub name: String,
    /// Exact byte length.
    pub bytes: u64,
    /// SHA-256 of the whole object, lowercase hex — verified by read-back
    /// before the manifest was published, and by `verify --deep` after.
    pub sha256: String,
    /// SHA-256 per [`RemoteManifest::chunk_bytes`] chunk, lowercase hex,
    /// in order; the last chunk may be short. Every ranged read verifies
    /// the chunks it touches against this table.
    pub chunks: Vec<String>,
}

/// One immutable remote snapshot, completely described.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteManifest {
    /// [`REMOTE_FORMAT`]. Anything else is refused, not interpreted.
    pub format: u32,
    /// The snapshot's id — its object-key component, restated inside the
    /// bytes so a copied prefix cannot silently masquerade.
    pub snapshot: String,
    /// The publishing store's declared identity (see
    /// [`super::RemoteOptions::store_identity`]).
    pub store: String,
    /// Wall-clock publication time, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
    /// The pinned generation this snapshot archives.
    pub source_generation: u64,
    /// The segment format of the archived files
    /// ([`crate::segment::VERSION`] at publication).
    pub segment_format: u16,
    /// Chunk size the per-object digest tables are stated in.
    pub chunk_bytes: u32,
    /// Every archived file, sorted by path.
    pub files: Vec<RemoteFile>,
    /// Every pack object this snapshot owns.
    pub objects: Vec<RemoteObject>,
}

impl RemoteManifest {
    /// Serializes for upload, enforcing the size bound the reader enforces.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| Error::Corrupt(format!("manifest does not encode: {error}")))?;
        if bytes.len() > super::MAX_MANIFEST_BYTES {
            return Err(Error::Corrupt(format!(
                "manifest is {} bytes, over the {} bound",
                bytes.len(),
                super::MAX_MANIFEST_BYTES
            )));
        }
        Ok(bytes)
    }

    /// Parses and fully validates manifest bytes fetched from the remote.
    /// `snapshot_id` is the id the caller addressed; the manifest must agree.
    pub(crate) fn decode(bytes: &[u8], snapshot_id: &str) -> Result<Self> {
        if bytes.len() > super::MAX_MANIFEST_BYTES {
            return Err(Error::Corrupt(format!(
                "manifest is {} bytes, over the {} bound",
                bytes.len(),
                super::MAX_MANIFEST_BYTES
            )));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| Error::Corrupt(format!("manifest does not decode: {error}")))?;
        manifest.validate(snapshot_id)?;
        Ok(manifest)
    }

    fn validate(&self, snapshot_id: &str) -> Result<()> {
        let refuse = |what: String| Err(Error::Corrupt(format!("manifest: {what}")));
        if self.format != REMOTE_FORMAT {
            return refuse(format!(
                "format {} where this build reads {REMOTE_FORMAT}",
                self.format
            ));
        }
        if self.snapshot != snapshot_id {
            return refuse(format!(
                "names snapshot {:?} but was fetched as {snapshot_id:?}",
                self.snapshot
            ));
        }
        if !self.chunk_bytes.is_power_of_two()
            || self.chunk_bytes < (64 << 10)
            || self.chunk_bytes > (16 << 20)
        {
            return refuse(format!(
                "chunk size {} outside 64 KiB..16 MiB",
                self.chunk_bytes
            ));
        }
        if self.files.len() > MAX_FILES {
            return refuse(format!(
                "{} files, over the {MAX_FILES} bound",
                self.files.len()
            ));
        }
        if self.objects.len() > MAX_OBJECTS {
            return refuse(format!(
                "{} objects, over the {MAX_OBJECTS} bound",
                self.objects.len()
            ));
        }
        for (index, object) in self.objects.iter().enumerate() {
            if object.name != object_name(index) {
                return refuse(format!(
                    "object {index} is named {:?}, not its canonical name",
                    object.name
                ));
            }
            // Zero-byte objects are legal: a pack holding only empty files
            // (an empty eval log cut off from its neighbours by an
            // oversized segment) is empty, carries zero chunk digests, and
            // still verifies by length and whole-object digest.
            if !valid_sha256_hex(&object.sha256) {
                return refuse(format!("object {index} digest is not 64 hex bytes"));
            }
            let expect_chunks = object.bytes.div_ceil(u64::from(self.chunk_bytes));
            if object.chunks.len() as u64 != expect_chunks {
                return refuse(format!(
                    "object {index} declares {} chunk digests where its length needs {expect_chunks}",
                    object.chunks.len()
                ));
            }
            if object.chunks.iter().any(|chunk| !valid_sha256_hex(chunk)) {
                return refuse(format!("object {index} carries a malformed chunk digest"));
            }
        }
        if self.segment_format != crate::segment::VERSION {
            return refuse(format!(
                "segment format v{} where this build reads v{}",
                self.segment_format,
                crate::segment::VERSION
            ));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut state_manifests = 0usize;
        let mut log_counts = [0usize; 3];
        // Per-object file ranges, for the coverage check below.
        let mut ranges: Vec<Vec<(u64, u64)>> = vec![Vec::new(); self.objects.len()];
        for file in &self.files {
            if !valid_archive_path(&file.path) {
                return refuse(format!(
                    "file path {:?} is not a safe relative path",
                    file.path
                ));
            }
            let kind = match classify_archive_path(&file.path) {
                Some(kind) => kind,
                None => {
                    return refuse(format!(
                        "file path {:?} is not a canonical pin file name",
                        file.path
                    ))
                }
            };
            match kind {
                ArchiveKind::StateManifest => state_manifests += 1,
                ArchiveKind::AnnotationLog => log_counts[0] += 1,
                ArchiveKind::EvalLog => log_counts[1] += 1,
                ArchiveKind::TombstoneLog => log_counts[2] += 1,
                ArchiveKind::Segment | ArchiveKind::Payload => {}
            }
            if !seen.insert(file.path.as_str()) {
                return refuse(format!("file path {:?} appears twice", file.path));
            }
            if !valid_sha256_hex(&file.sha256) {
                return refuse(format!("file {:?} digest is not 64 hex bytes", file.path));
            }
            let object = self.objects.get(file.object as usize).ok_or_else(|| {
                Error::Corrupt(format!(
                    "manifest: file {:?} names object {} of {}",
                    file.path,
                    file.object,
                    self.objects.len()
                ))
            })?;
            if file
                .offset
                .checked_add(file.bytes)
                .filter(|end| *end <= object.bytes)
                .is_none()
            {
                return refuse(format!("file {:?} extends past its object", file.path));
            }
            ranges[file.object as usize].push((file.offset, file.bytes));
        }
        if state_manifests != 1 {
            return refuse(format!(
                "{state_manifests} {STATE_MANIFEST_FILE} entries where exactly one is required"
            ));
        }
        if log_counts.iter().any(|count| *count > 1) {
            return refuse("a log file appears more than once".to_owned());
        }
        // Coverage: within every object the nonempty file ranges, sorted,
        // must tile exactly [0, object.bytes] with no gaps and no overlaps
        // — bytes no file claims and bytes two files share are both shapes
        // no publisher writes and a substitution needs. Zero-byte files
        // must sit at a covered boundary.
        for (index, object) in self.objects.iter().enumerate() {
            let mut nonempty: Vec<(u64, u64)> = ranges[index]
                .iter()
                .copied()
                .filter(|(_, bytes)| *bytes > 0)
                .collect();
            nonempty.sort_unstable();
            let mut cursor = 0u64;
            for (offset, bytes) in &nonempty {
                if *offset != cursor {
                    return refuse(format!(
                        "object {index} has a gap or overlap at byte {cursor}"
                    ));
                }
                cursor += bytes;
            }
            if cursor != object.bytes {
                return refuse(format!(
                    "object {index} holds {} bytes but its files account for {cursor}",
                    object.bytes
                ));
            }
            for (offset, bytes) in &ranges[index] {
                if *bytes == 0 && *offset > object.bytes {
                    return refuse(format!("object {index} places an empty file past its end"));
                }
            }
        }
        Ok(())
    }

    /// Cross-checks this remote manifest against the ARCHIVED generation
    /// manifest it carries — the binding that makes omission and
    /// substitution loud. The remote file set minus the state-manifest
    /// entry must equal the generation manifest's file set exactly, by
    /// `(path, bytes, sha256)`; the archived generation id and segment
    /// format must match what the remote manifest claims. A remote manifest
    /// that lists fewer, more, or different files than the generation it
    /// embeds is a tamper or a corruption, and a query over it would
    /// otherwise be a silently partial store.
    pub(crate) fn crosscheck_generation(
        &self,
        generation: &crate::generation::Manifest,
    ) -> Result<()> {
        let refuse = |what: String| {
            Err(Error::Corrupt(format!(
                "remote manifest disagrees with its archived generation manifest: {what}"
            )))
        };
        if generation.generation != self.source_generation {
            return refuse(format!(
                "generation {} archived, {} claimed",
                generation.generation, self.source_generation
            ));
        }
        match generation.segment_format {
            Some(format) if format == self.segment_format => {}
            Some(format) => {
                return refuse(format!(
                    "segment format v{format} archived, v{} claimed",
                    self.segment_format
                ))
            }
            None => {
                return refuse(
                    "the archived generation declares no segment format (pre-v8 pin)".to_owned(),
                )
            }
        }
        let mut remote: Vec<(&str, u64, &str)> = self
            .files
            .iter()
            .filter(|file| file.path != STATE_MANIFEST_FILE)
            .map(|file| (file.path.as_str(), file.bytes, file.sha256.as_str()))
            .collect();
        remote.sort_unstable();
        let mut archived: Vec<(&str, u64, &str)> = generation
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.bytes, file.sha256.as_str()))
            .collect();
        archived.sort_unstable();
        if remote != archived {
            // Name the first divergence — an operator debugging a damaged
            // archive needs the file, not just the fact.
            for (ours, theirs) in remote.iter().zip(archived.iter()) {
                if ours != theirs {
                    return refuse(format!(
                        "first divergence at {:?} vs {:?}",
                        ours.0, theirs.0
                    ));
                }
            }
            return refuse(format!(
                "{} remote files vs {} archived",
                remote.len(),
                archived.len()
            ));
        }
        Ok(())
    }

    /// The file entry at `path`, if any.
    pub(crate) fn file(&self, path: &str) -> Option<(usize, &RemoteFile)> {
        self.files
            .iter()
            .enumerate()
            .find(|(_, file)| file.path == path)
    }

    /// Total bytes across the snapshot's objects.
    pub(crate) fn total_object_bytes(&self) -> u64 {
        self.objects.iter().map(|object| object.bytes).sum()
    }
}

/// The canonical name of pack object `index`.
pub(crate) fn object_name(index: usize) -> String {
    format!("{}/{index:08}.pack", super::OBJECTS_DIR)
}

/// Parses ARCHIVED generation-manifest bytes, bounded. The caller fetched
/// them through the chunk-verified path; this only turns them into the
/// structure [`RemoteManifest::crosscheck_generation`] compares against.
pub(crate) fn parse_generation_manifest(bytes: &[u8]) -> Result<crate::generation::Manifest> {
    if bytes.len() > super::MAX_MANIFEST_BYTES {
        return Err(Error::Corrupt(format!(
            "archived generation manifest is {} bytes, over the {} bound",
            bytes.len(),
            super::MAX_MANIFEST_BYTES
        )));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        Error::Corrupt(format!(
            "archived generation manifest does not decode: {error}"
        ))
    })
}

/// Whether `path` is a safe, canonical, relative archive path: `/`-separated
/// components, each non-empty, never `.` or `..`, never hidden, drawn from
/// `[A-Za-z0-9._-]`, total length bounded. This is what stands between a
/// hostile manifest and a restore writing outside its staging directory.
pub(crate) fn valid_archive_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    path.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && !component.starts_with('.')
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    })
}

/// Whether `digest` is 64 lowercase hex bytes.
pub(crate) fn valid_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> RemoteManifest {
        let body = b"hello".to_vec();
        let sha = crate::payload::sha256_hex(&body);
        RemoteManifest {
            format: REMOTE_FORMAT,
            snapshot: "snap".to_owned(),
            store: "s".to_owned(),
            created_unix_ns: 0,
            source_generation: 1,
            segment_format: crate::segment::VERSION,
            chunk_bytes: super::super::CHUNK_BYTES,
            files: vec![RemoteFile {
                path: STATE_MANIFEST_FILE.to_owned(),
                bytes: body.len() as u64,
                sha256: sha.clone(),
                object: 0,
                offset: 0,
            }],
            objects: vec![RemoteObject {
                name: object_name(0),
                bytes: body.len() as u64,
                sha256: sha.clone(),
                chunks: vec![sha],
            }],
        }
    }

    #[test]
    fn a_valid_manifest_round_trips() {
        let manifest = minimal();
        let bytes = manifest.encode().expect("encode");
        let decoded = RemoteManifest::decode(&bytes, "snap").expect("decode");
        assert_eq!(decoded.files.len(), 1);
    }

    #[test]
    fn traversal_and_malformed_paths_are_refused() {
        assert!(valid_archive_path("segment-00000000000000000001.seg"));
        assert!(valid_archive_path("payloads/ab/cdef.bin"));
        assert!(!valid_archive_path("../evil"));
        assert!(!valid_archive_path("a/../b"));
        assert!(!valid_archive_path("/rooted"));
        assert!(!valid_archive_path("a//b"));
        assert!(!valid_archive_path(".hidden"));
        assert!(!valid_archive_path("has space"));
        assert!(!valid_archive_path(""));

        let mut manifest = minimal();
        manifest.files[0].path = "../evil".to_owned();
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());
    }

    #[test]
    fn geometry_lies_are_refused() {
        // A file extending past its object.
        let mut manifest = minimal();
        manifest.files[0].bytes += 1;
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());

        // A chunk table that does not match the object length.
        let mut manifest = minimal();
        manifest.objects[0].chunks.clear();
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());

        // A wrong snapshot id inside the bytes.
        let manifest = minimal();
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "other").is_err());

        // A foreign format generation.
        let mut manifest = minimal();
        manifest.format = 999;
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());

        // A missing state-manifest entry.
        let mut manifest = minimal();
        manifest.files[0].path = "annotations.jsonl".to_owned();
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());

        // A foreign segment format.
        let mut manifest = minimal();
        manifest.segment_format = 7;
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        assert!(RemoteManifest::decode(&bytes, "snap").is_err());
    }

    #[test]
    fn only_canonical_pin_names_classify() {
        assert_eq!(
            classify_archive_path("segment-00000000000000000042.seg"),
            Some(ArchiveKind::Segment)
        );
        assert_eq!(
            classify_archive_path(&format!("payloads/ab/ab{}.bin", "0".repeat(62))),
            Some(ArchiveKind::Payload)
        );
        assert_eq!(
            classify_archive_path("tombstones.jsonl"),
            Some(ArchiveKind::TombstoneLog)
        );
        // Wrong digit width, non-numeric, and u64 overflow all fail.
        assert_eq!(classify_archive_path("segment-42.seg"), None);
        assert_eq!(
            classify_archive_path("segment-0000000000000000004x.seg"),
            None
        );
        assert_eq!(
            classify_archive_path("segment-99999999999999999999.seg"),
            None
        );
        // Wrong shard, wrong hash width, uppercase.
        assert_eq!(
            classify_archive_path(&format!("payloads/cd/ab{}.bin", "0".repeat(62))),
            None
        );
        assert_eq!(classify_archive_path("payloads/ab/short.bin"), None);
        assert_eq!(
            classify_archive_path(&format!("payloads/AB/AB{}.bin", "0".repeat(62))),
            None
        );
        // Foreign but syntactically safe names are still refused.
        assert_eq!(classify_archive_path("extra.bin"), None);
        assert_eq!(classify_archive_path("wal.log"), None);
        assert_eq!(classify_archive_path("CURRENT"), None);
        assert_eq!(
            classify_archive_path("segment-00000000000000000001.seg.rollup"),
            None
        );
    }

    #[test]
    fn pack_coverage_must_tile_exactly() {
        // Two files must account for every object byte: a gap (dropped
        // entry) and an overlap (substituted entry) are both refused.
        let body_a = b"aaaa".to_vec();
        let body_b = b"bbbb".to_vec();
        let file = |path: &str, bytes: &[u8], offset: u64| RemoteFile {
            path: path.to_owned(),
            bytes: bytes.len() as u64,
            sha256: crate::payload::sha256_hex(bytes),
            object: 0,
            offset,
        };
        let mut manifest = minimal();
        let mut whole = body_a.clone();
        whole.extend_from_slice(&body_b);
        manifest.objects[0] = RemoteObject {
            name: object_name(0),
            bytes: whole.len() as u64,
            sha256: crate::payload::sha256_hex(&whole),
            chunks: vec![crate::payload::sha256_hex(&whole)],
        };
        manifest.files = vec![
            file(STATE_MANIFEST_FILE, &body_a, 0),
            file("annotations.jsonl", &body_b, 4),
        ];
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        RemoteManifest::decode(&bytes, "snap").expect("exact tiling is accepted");

        // Gap: drop the second file.
        let mut gapped = manifest.clone();
        gapped.files.pop();
        let bytes = serde_json::to_vec(&gapped).expect("encode");
        assert!(
            RemoteManifest::decode(&bytes, "snap").is_err(),
            "gap accepted"
        );

        // Overlap: both files claim offset 0.
        let mut overlapped = manifest.clone();
        overlapped.files[1].offset = 0;
        let bytes = serde_json::to_vec(&overlapped).expect("encode");
        assert!(
            RemoteManifest::decode(&bytes, "snap").is_err(),
            "overlap accepted"
        );
    }

    #[test]
    fn the_generation_crosscheck_catches_omission_and_substitution() {
        let seg = b"segment bytes".to_vec();
        let seg_name = "segment-00000000000000000001.seg";
        let generation = crate::generation::Manifest {
            generation: 7,
            created_unix_ns: 0,
            folded_through: crate::generation::FoldedThrough::NONE,
            files: vec![crate::generation::ManifestFile {
                path: seg_name.to_owned(),
                bytes: seg.len() as u64,
                sha256: crate::payload::sha256_hex(&seg),
                modified_unix_ns: 0,
            }],
            segment_format: Some(crate::segment::VERSION),
        };
        let mut manifest = minimal();
        manifest.source_generation = 7;
        manifest.files.push(RemoteFile {
            path: seg_name.to_owned(),
            bytes: seg.len() as u64,
            sha256: crate::payload::sha256_hex(&seg),
            object: 0,
            offset: 0,
        });
        manifest
            .crosscheck_generation(&generation)
            .expect("exact set agrees");

        // Omission: the remote manifest lists no segment at all — a query
        // over it would silently answer from a partial store.
        let mut omitted = manifest.clone();
        omitted.files.retain(|file| file.path != seg_name);
        assert!(omitted.crosscheck_generation(&generation).is_err());

        // Substitution: same shape, different name.
        let mut substituted = manifest.clone();
        substituted
            .files
            .iter_mut()
            .filter(|file| file.path == seg_name)
            .for_each(|file| file.path = "segment-00000000000000000099.seg".to_owned());
        assert!(substituted.crosscheck_generation(&generation).is_err());

        // Wrong claimed generation, and a pre-v8 pin with no declared
        // format.
        let mut wrong_generation = manifest.clone();
        wrong_generation.source_generation = 8;
        assert!(wrong_generation.crosscheck_generation(&generation).is_err());
        let mut undeclared = generation.clone();
        undeclared.segment_format = None;
        assert!(manifest.crosscheck_generation(&undeclared).is_err());
    }
}
