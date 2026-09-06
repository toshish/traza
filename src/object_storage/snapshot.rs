//! The remote reader: a snapshot's segments opened over ranged object reads.
//!
//! Opening fetches and validates the manifest, then opens every archived
//! segment through [`crate::segment::Segment::open_from_source`] — the SAME
//! validation and index decoding a local open runs, with the file handle
//! replaced by a chunk-verified remote range source. Record bytes stay
//! remote; queries fetch exactly the chunks their block reads touch, through
//! one globally bounded, evicting cache shared by all of the snapshot's
//! segments.
//!
//! Query semantics are the engine's own, verbatim: the snapshot builds an
//! empty write buffer plus the recency-ordered segment list and hands them
//! to the crate-root `query_view`, so last-write-wins across segments,
//! tenant scoping, filters, cursors and content search answer exactly as the
//! source store would have answered at pin time. Publication refused pending
//! erasures, so no mask applies: everything the snapshot holds was queryable
//! state.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::manifest::RemoteManifest;
use super::{Error, Remote, Result};
use crate::segment::RangeSource;
use crate::{Span, SpanCursor, SpanFilter};

/// The globally bounded raw chunk cache one snapshot's segments share.
///
/// Keys are `(object index, chunk index)`; values are whole verified-on-
/// every-read chunks. Eviction is least-recently-used, and the budget is
/// bytes, not entries, so oversized final chunks cannot cheat it. The cache
/// stores raw bytes and every read — hit or miss — re-verifies the chunk's
/// SHA-256 against the manifest before believing it, so even in-memory
/// tampering surfaces as `Corrupt` rather than as served data.
pub(crate) struct ChunkCache {
    budget: usize,
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    /// MRU-first, like the segment block cache: entries are few (budget /
    /// chunk size), so a vector scan is cheaper than it looks.
    slots: Vec<((u32, u32), Vec<u8>)>,
    resident: usize,
}

impl ChunkCache {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            inner: Mutex::new(CacheInner {
                slots: Vec::new(),
                resident: 0,
            }),
        }
    }

    fn get(&self, key: (u32, u32)) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().ok()?;
        let position = inner.slots.iter().position(|(held, _)| *held == key)?;
        let hit = inner.slots.remove(position);
        let bytes = hit.1.clone();
        inner.slots.insert(0, hit);
        Some(bytes)
    }

    fn insert(&self, key: (u32, u32), bytes: Vec<u8>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if bytes.len() > self.budget {
            // A chunk that alone exceeds the budget is served uncached.
            return;
        }
        if let Some(position) = inner.slots.iter().position(|(held, _)| *held == key) {
            let (_, old) = inner.slots.remove(position);
            inner.resident -= old.len();
        }
        inner.resident += bytes.len();
        inner.slots.insert(0, (key, bytes));
        while inner.resident > self.budget {
            let Some((_, evicted)) = inner.slots.pop() else {
                break;
            };
            inner.resident -= evicted.len();
        }
    }

    fn drop_key(&self, key: (u32, u32)) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(position) = inner.slots.iter().position(|(held, _)| *held == key) {
            let (_, old) = inner.slots.remove(position);
            inner.resident -= old.len();
        }
    }

    fn resident_bytes(&self) -> usize {
        self.inner.lock().map(|inner| inner.resident).unwrap_or(0)
    }

    /// Test hook: flips one byte of one cached chunk in place, returning
    /// whether anything was there to tamper with. Exists so the "a cache is
    /// not a trust boundary" property is testable from outside the crate.
    fn tamper_one(&self) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        match inner.slots.first_mut() {
            Some((_, bytes)) if !bytes.is_empty() => {
                bytes[0] ^= 0xff;
                true
            }
            _ => false,
        }
    }
}

/// Everything a remote read needs, shared by every segment's source.
pub(crate) struct Fetch {
    remote: Remote,
    snapshot_id: String,
    manifest: Arc<RemoteManifest>,
    cache: ChunkCache,
}

impl Fetch {
    /// One verified chunk of `object`, from cache or remote. The digest
    /// check runs on EVERY return path, cache hits included.
    fn chunk(&self, object_index: u32, chunk_index: u32) -> Result<Vec<u8>> {
        let object = self
            .manifest
            .objects
            .get(object_index as usize)
            .ok_or_else(|| Error::Corrupt("chunk read names an unknown object".to_owned()))?;
        let chunk_bytes = u64::from(self.manifest.chunk_bytes);
        let start = u64::from(chunk_index) * chunk_bytes;
        let end = (start + chunk_bytes).min(object.bytes);
        if start >= object.bytes {
            return Err(Error::Corrupt("chunk read past the object".to_owned()));
        }
        let expected = object
            .chunks
            .get(chunk_index as usize)
            .ok_or_else(|| Error::Corrupt("chunk digest missing from manifest".to_owned()))?;
        let key = (object_index, chunk_index);
        if let Some(bytes) = self.cache.get(key) {
            if crate::payload::sha256_hex(&bytes) == *expected {
                self.remote
                    .counters()
                    .cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(bytes);
            }
            // A cached chunk failing its digest is memory damage or
            // tampering; drop it and refuse rather than silently refetch —
            // whoever can rewrite this cache can rewrite anything.
            self.cache.drop_key(key);
            return Err(Error::Corrupt(format!(
                "{}: cached chunk {chunk_index} failed its digest",
                object.name
            )));
        }
        let location = self.remote.object_path(&self.snapshot_id, &object.name);
        let bytes = self.remote.get_range(&location, start..end)?;
        self.remote
            .counters()
            .chunk_fetches
            .fetch_add(1, Ordering::Relaxed);
        if crate::payload::sha256_hex(&bytes) != *expected {
            return Err(Error::Corrupt(format!(
                "{}: chunk {chunk_index} failed its digest",
                object.name
            )));
        }
        self.cache.insert(key, bytes.clone());
        Ok(bytes)
    }

    /// An arbitrary byte range of one archived FILE, assembled from verified
    /// chunks of its object.
    pub(crate) fn file_range(&self, file_index: usize, start: u64, len: u64) -> Result<Vec<u8>> {
        let file = self
            .manifest
            .files
            .get(file_index)
            .ok_or_else(|| Error::Corrupt("file read names an unknown entry".to_owned()))?;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= file.bytes)
            .ok_or_else(|| Error::Corrupt(format!("{}: range read outside the file", file.path)))?;
        let chunk_bytes = u64::from(self.manifest.chunk_bytes);
        let absolute_start = file.offset + start;
        let absolute_end = file.offset + end;
        let mut assembled = Vec::with_capacity(len as usize);
        let mut chunk_index = absolute_start / chunk_bytes;
        while chunk_index * chunk_bytes < absolute_end {
            let chunk = self.chunk(file.object, chunk_index as u32)?;
            let chunk_start = chunk_index * chunk_bytes;
            let from = absolute_start.max(chunk_start) - chunk_start;
            let to = absolute_end.min(chunk_start + chunk.len() as u64) - chunk_start;
            if to > chunk.len() as u64 {
                return Err(Error::Corrupt(format!(
                    "{}: chunk shorter than the range needs",
                    file.path
                )));
            }
            assembled.extend_from_slice(&chunk[from as usize..to as usize]);
            chunk_index += 1;
        }
        if assembled.len() as u64 != len {
            return Err(Error::Corrupt(format!(
                "{}: range assembly is short",
                file.path
            )));
        }
        Ok(assembled)
    }

    /// One whole archived file, chunk-verified AND whole-file digest
    /// verified against its manifest entry.
    pub(crate) fn whole_file(&self, file_index: usize) -> Result<Vec<u8>> {
        let file = &self.manifest.files[file_index];
        let bytes = self.file_range(file_index, 0, file.bytes)?;
        if crate::payload::sha256_hex(&bytes) != file.sha256 {
            return Err(Error::Corrupt(format!(
                "{}: whole-file digest mismatch",
                file.path
            )));
        }
        Ok(bytes)
    }
}

/// A [`RangeSource`] over one archived segment file.
struct RemoteFileSource {
    fetch: Arc<Fetch>,
    file_index: usize,
    /// The file's path and length, copied out for Debug and bounds.
    path: String,
    bytes: u64,
}

impl std::fmt::Debug for RemoteFileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteFileSource({}, {} bytes)", self.path, self.bytes)
    }
}

impl RangeSource for RemoteFileSource {
    fn read_range(&self, start: u64, len: u64) -> io::Result<Vec<u8>> {
        self.fetch
            .file_range(self.file_index, start, len)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }

    fn total_len(&self) -> u64 {
        self.bytes
    }
}

/// I/O and residency truth for one open snapshot. See
/// [`RemoteSnapshot::read_stats`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadStats {
    /// Chunk fetches that went to the remote.
    pub chunk_fetches: u64,
    /// Bytes fetched from the remote by this [`Remote`] (all snapshots and
    /// admin operations it served).
    pub fetched_bytes: u64,
    /// Chunk reads served by the bounded cache.
    pub cache_hits: u64,
    /// Bytes the bounded chunk cache currently holds — the number its
    /// budget governs.
    pub cache_resident_bytes: u64,
    /// Approximate resident bytes of the eagerly decoded segment metadata
    /// indexes. **Not bounded by the chunk-cache budget** — exactly like a
    /// local open, this scales with index cardinality and lives as long as
    /// the snapshot.
    pub resident_index_bytes: u64,
    /// Bytes the per-segment decoded-block caches hold (bounded per
    /// segment, as locally).
    pub block_cache_bytes: u64,
    /// Total bytes of the snapshot's remote objects — the download a full
    /// hydration would have cost, for comparison against `fetched_bytes`.
    pub remote_total_bytes: u64,
}

/// One immutable remote snapshot, open for querying.
///
/// Constructed by [`Remote::open_snapshot`]. Queries run the engine's own
/// resolution over the archived segments; no local store, directory, or
/// lock is involved, and nothing here can write.
/// Deletion prevents new opens but does not revoke this handle's cached data.
/// Discard open handles when retiring an archive; in-flight reads may fail.
pub struct RemoteSnapshot {
    fetch: Arc<Fetch>,
    buffer: crate::WriteBuffer,
    segments: Vec<Arc<crate::Segment>>,
    metrics: crate::metrics::Metrics,
    pricing: crate::pricing::Pricing,
    /// `sha256/<hex>` reference → file index, for payload retrieval.
    payload_files: HashMap<String, usize>,
    /// SHA-256 of the manifest bytes this snapshot was opened from.
    manifest_sha256: String,
}

impl std::fmt::Debug for RemoteSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSnapshot")
            .field("snapshot", &self.fetch.manifest.snapshot)
            .field("segments", &self.segments.len())
            .finish_non_exhaustive()
    }
}

pub(crate) fn open(remote: &Remote, snapshot_id: &str) -> Result<RemoteSnapshot> {
    // The manifest gate: tombstone, expected external digest, structural
    // validation, store identity.
    let (manifest, manifest_sha256) =
        super::admin::fetch_manifest_with_digest(remote, snapshot_id)?;
    // The archived-state binding: the manifest must agree exactly with the
    // generation manifest it archived (no omitted or substituted segment
    // can survive this), and the archived tombstone log must strictly parse
    // with zero pending erasures — queries here run with NO mask, so a
    // pending record would mean serving spans the source store was
    // masking.
    super::admin::crosscheck_archived_state(remote, &manifest, snapshot_id)?;
    let fetch = Arc::new(Fetch {
        remote: remote.clone(),
        snapshot_id: snapshot_id.to_owned(),
        manifest: Arc::new(manifest),
        cache: ChunkCache::new(remote.cache_budget()),
    });

    let mut segments: Vec<Arc<crate::Segment>> = Vec::new();
    let mut payload_files: HashMap<String, usize> = HashMap::new();
    for (index, file) in fetch.manifest.files.iter().enumerate() {
        if file.path.starts_with("segment-") && file.path.ends_with(".seg") {
            let source = Box::new(RemoteFileSource {
                fetch: Arc::clone(&fetch),
                file_index: index,
                path: file.path.clone(),
                bytes: file.bytes,
            });
            let seg = crate::segment::Segment::open_from_source(source)
                .map_err(|error| Error::Corrupt(format!("{}: {error}", file.path)))?;
            segments.push(Arc::new(crate::Segment {
                // The ORIGINAL filename, kept for exactly one purpose:
                // recency order is path order, and last-write-wins reads
                // depend on it. Never sorted by content, never dereferenced
                // as a local path (`local_sidecars: false` guarantees the
                // latter).
                path: std::path::PathBuf::from(&file.path),
                bytes: file.bytes,
                seg: Box::new(seg),
                key_hashes: std::sync::OnceLock::new(),
                local_sidecars: false,
            }));
        } else if let Some(rest) = file.path.strip_prefix("payloads/") {
            // payloads/<shard>/<hash>.bin → "sha256/<hash>".
            if let Some(name) = rest.split('/').nth(1) {
                if let Some(hash) = name.strip_suffix(".bin") {
                    payload_files.insert(format!("sha256/{hash}"), index);
                }
            }
        }
    }
    segments.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(RemoteSnapshot {
        fetch,
        buffer: crate::WriteBuffer::default(),
        segments,
        metrics: crate::metrics::Metrics::default(),
        pricing: crate::pricing::Pricing::default(),
        payload_files,
        manifest_sha256,
    })
}

impl RemoteSnapshot {
    /// The snapshot's validated manifest.
    pub fn manifest(&self) -> &RemoteManifest {
        &self.fetch.manifest
    }

    /// SHA-256 (lowercase hex) of the manifest bytes this snapshot was
    /// opened from — compare against the digest retained from
    /// [`super::PublishReceipt::manifest_sha256`], or pin it up front via
    /// [`super::RemoteOptions::expected_manifest_sha256`].
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// Spans matching `filter`, in the engine's stable span order, resolved
    /// with the engine's own semantics: last-write-wins across segments,
    /// tenant scoping, content search, sorting and limits — exactly what the
    /// source store would have answered at pin time.
    pub fn query(&self, filter: &SpanFilter) -> crate::Result<Vec<Span>> {
        self.query_after(filter, None)
    }

    /// [`Self::query`] strictly after `cursor` — the same bounded
    /// pagination export uses locally.
    pub fn query_after(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
    ) -> crate::Result<Vec<Span>> {
        self.query_bounded(filter, cursor, None)
    }

    /// [`Self::query_after`] with cooperative deadline checks and a final
    /// expiry check. An expired query returns `DeadlineExceeded`, never a
    /// successful answer.
    ///
    /// **Precision:** the budget is a COMPUTE deadline, observed at segment
    /// boundaries, every few thousand decoded records, and once before the
    /// answer returns. An in-flight segment operation may issue several
    /// remote requests before its next checkpoint. Each request is bounded
    /// by [`super::RemoteOptions::op_timeout`], but this budget is not a
    /// strict wall-clock cancellation bound. Requests sharing a `Remote`
    /// also wait for serialized transport admission.
    pub fn query_bounded(
        &self,
        filter: &SpanFilter,
        cursor: Option<&SpanCursor>,
        budget: Option<std::time::Duration>,
    ) -> crate::Result<Vec<Span>> {
        let deadline = crate::Deadline::starting(Instant::now(), budget);
        if let Some(session_id) = &filter.session {
            let spans = crate::analytics::resolve_session_spans_in(
                &self.buffer,
                &self.segments,
                filter.tenant.as_deref(),
                session_id,
                None,
                deadline,
            )?;
            let spans = crate::narrow_session_spans(spans, filter, cursor);
            // Same rule as every engine path: a complete answer finished
            // past its budget is refused, not rewarded.
            crate::Deadline::check(deadline, self.segments.len() as u32)?;
            return Ok(spans);
        }
        crate::query_view(
            &self.buffer,
            &self.segments,
            &self.metrics,
            &self.pricing,
            filter,
            cursor,
            None,
            deadline,
        )
    }

    /// Every span of `trace_id`, tenant-scoped like the live store's
    /// operator/tenant views: `None` returns all tenants' spans under the
    /// id, `Some(t)` only tenant `t`'s. Last-write-wins across segments,
    /// ordered by start time.
    pub fn get_trace(&self, tenant: Option<&str>, trace_id: &str) -> crate::Result<Vec<Span>> {
        self.get_trace_bounded(tenant, trace_id, None)
    }

    /// [`Self::get_trace`] under a compute budget, with the same expiry
    /// rule as [`Self::query_bounded`]: checked per segment and once
    /// before the trace returns, never a success after the budget ran out.
    pub fn get_trace_bounded(
        &self,
        tenant: Option<&str>,
        trace_id: &str,
        budget: Option<std::time::Duration>,
    ) -> crate::Result<Vec<Span>> {
        let deadline = crate::Deadline::starting(Instant::now(), budget);
        let in_scope = |span: &Span| tenant.map_or(true, |tenant| span.tenant.as_str() == tenant);
        let mut latest: HashMap<(String, String), Span> = HashMap::new();
        let mut segments_examined: u32 = 0;
        for segment in self.segments.iter() {
            crate::Deadline::check(deadline, segments_examined)?;
            segments_examined += 1;
            for span in segment.trace_spans(trace_id)? {
                if in_scope(&span) {
                    latest.insert((span.tenant.clone(), span.span_id.clone()), span);
                }
            }
        }
        let mut result: Vec<Span> = latest.into_values().collect();
        crate::sort_spans(&mut result);
        crate::Deadline::check(deadline, segments_examined)?;
        Ok(result)
    }

    /// The decoded bytes of an offloaded payload by its `sha256/<hex>`
    /// reference, or `None` when the snapshot holds no such file. The blob
    /// is fetched by verified chunks, checked against its manifest digest,
    /// decoded through the payload framing (CRC and length checks), and the
    /// decoded content re-hashed against the reference — the full local
    /// verification story, remotely.
    pub fn payload(&self, reference: &str) -> Result<Option<Vec<u8>>> {
        let Some(&file_index) = self.payload_files.get(reference) else {
            return Ok(None);
        };
        let encoded = self.fetch.whole_file(file_index)?;
        let decoded = crate::payload::decode_blob(&encoded, reference)
            .map_err(|error| Error::Corrupt(error.to_string()))?;
        let Some(hash) = reference.strip_prefix("sha256/") else {
            return Ok(None);
        };
        if crate::payload::sha256_hex(&decoded) != hash {
            return Err(Error::Corrupt(format!(
                "payload {reference}: decoded content does not match its address"
            )));
        }
        Ok(Some(decoded))
    }

    /// The number of archived segments this snapshot serves.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// A verified range of one archived file — restore's streaming read.
    pub(crate) fn fetch_file_range(
        &self,
        file_index: usize,
        start: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        self.fetch.file_range(file_index, start, len)
    }

    /// Transfer and residency truth: what has been fetched, what the
    /// bounded cache holds, and the two residencies the cache does NOT
    /// bound (decoded metadata indexes, per-segment block caches).
    pub fn read_stats(&self) -> ReadStats {
        let counters = self.fetch.remote.counters();
        ReadStats {
            chunk_fetches: counters.chunk_fetches.load(Ordering::Relaxed),
            fetched_bytes: counters.fetched_bytes.load(Ordering::Relaxed),
            cache_hits: counters.cache_hits.load(Ordering::Relaxed),
            cache_resident_bytes: self.fetch.cache.resident_bytes() as u64,
            resident_index_bytes: self
                .segments
                .iter()
                .map(|segment| segment.seg.approx_index_bytes() as u64)
                .sum(),
            block_cache_bytes: self
                .segments
                .iter()
                .map(|segment| segment.seg.resident_bytes() as u64)
                .sum(),
            remote_total_bytes: self.fetch.manifest.total_object_bytes(),
        }
    }

    /// Test hook: corrupts one cached chunk in memory, so tests can prove
    /// cache reads are digest-checked rather than trusted. Returns whether
    /// any chunk was cached to tamper with.
    #[doc(hidden)]
    pub fn tamper_cached_chunk_for_tests(&self) -> bool {
        self.fetch.cache.tamper_one()
    }
}
