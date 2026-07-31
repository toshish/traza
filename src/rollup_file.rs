//! On-disk persistence for a segment's analytics rollup.
//!
//! # Why this is a sidecar and not a segment section
//!
//! A [`crate::analytics::SegmentRollup`] is derived entirely from an immutable
//! segment, so it could have been written into the segment the way the trace,
//! attribute and content indexes are. It deliberately is not.
//!
//! The segment format describes STORAGE: records, offsets, and the indexes
//! that address them. A rollup describes ANALYTICS: what counts as an LLM
//! call, which attribute carries the model, how a cost is derived, which
//! session key wins when spans disagree. Those two things change on
//! completely different schedules — [`crate::semconv`] gains a convention
//! every time a producer ships one, while the segment format has moved six
//! times in the project's life. Putting the rollup inside the segment would
//! tie every semantic-convention change to a storage format bump, and a
//! storage format bump makes every existing segment on disk unreadable.
//!
//! As a sidecar it carries its own [`SCHEMA_VERSION`], and a rollup written
//! under an older analytics schema is simply rebuilt. That is the whole
//! argument: **a derived file may be wrong, but it must never be believed
//! when it is.** Every failure mode here — absent, truncated, stale, corrupt,
//! bound to a different segment — resolves to "rebuild it from the segment",
//! which is exactly the behaviour that existed before this file did.
//!
//! # Format
//!
//! Little-endian throughout, matching `src/segment.rs`.
//!
//! ```text
//! magic            8 bytes   "TRAZAROL"
//! format version   u16       FORMAT_VERSION
//! reserved         u16       zero
//! schema version   u32       SCHEMA_VERSION
//! segment bytes    u64  \
//! record count     u64   |   the BINDING: which segment this describes
//! min start ns     u64   |
//! max start ns     u64  /
//! min end ns       u64      span END range, for TTL expiry
//! max end ns       u64
//! prologue cksum   u64       FNV-1a over the 64 bytes above
//! by_model         counter map
//! by_provider      counter map
//! by_service       counter map
//! by_day           counter map
//! by_session_key   counter map
//! sessions         session map
//! key hashes       u64 count, then u64 each
//! payload refs     u32 count, then length-prefixed strings
//! checksum         u64       FNV-1a over every preceding byte
//! ```
//!
//! A counter map is a `u32` entry count followed by a length-prefixed key and
//! eight fixed-width counter fields. A session entry adds the first/last
//! timestamps, the trace-id set, and an index into
//! [`crate::semconv::SESSION_KEYS`] (`u32::MAX` for none).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::analytics::{Counters, SegmentRollup, SessionCounters};
use crate::semconv;

const MAGIC: [u8; 8] = *b"TRAZAROL";

/// Layout version of this file. Bump when the BYTES change.
const FORMAT_VERSION: u16 = 2;

/// Version of the analytics semantics the counters were computed under.
///
/// **Bump this whenever a rollup's VALUES would come out different for the
/// same spans** — a new recognized attribute in [`crate::semconv`], a changed
/// cost derivation, a reordered session-key precedence, a new grouping. The
/// counters are otherwise indistinguishable from correct ones: nothing about
/// a stale rollup looks wrong, it just quietly reports the old model's answer
/// forever. Forgetting to bump this is the one failure this file cannot
/// detect for you, which is why it is the first constant in the module.
const SCHEMA_VERSION: u32 = 1;

/// Extension of the sidecar written beside `segment-<id>.seg`.
const ROLLUP_SUFFIX: &str = "rollup";

/// Bytes of the fixed prologue, including its own checksum.
///
/// The prologue is separately checksummed so that [`bounds`] can answer from
/// one short read. TTL expiry asks that question about every segment on every
/// tick, and it must be able to trust the answer without reading — let alone
/// decoding — anything else: an unverified bound would let expiry skip a
/// segment that should have been swept, or sweep one that should not.
const PROLOGUE_LEN: usize = 72;

/// Offset of the prologue's own checksum, which covers everything before it.
const PROLOGUE_CHECKSUM_AT: usize = 64;

/// The timestamp ranges a rollup covers.
///
/// Start and end are tracked separately because they answer different
/// questions and do not bound each other: aggregation windows filter on
/// `start_time_ns`, TTL expires on `end_time_ns`, and a long span can end well
/// after the last span in its segment started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) min_start_ns: u64,
    pub(crate) max_start_ns: u64,
    pub(crate) min_end_ns: u64,
    pub(crate) max_end_ns: u64,
}

/// Distinguishes concurrent temp files, as `seal_segment` does for segments.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The sidecar path for a segment path.
pub(crate) fn rollup_path(segment_path: &Path) -> PathBuf {
    segment_path.with_extension(ROLLUP_SUFFIX)
}

/// Removes a segment's sidecar, treating "already gone" as success.
///
/// A sidecar that outlives its segment is not merely wasted space: segment
/// ids are not reused today, but if one ever were, an orphan would be a
/// rollup describing a DIFFERENT segment under the right name. The binding
/// check would catch it — this keeps it from arising.
pub(crate) fn remove(segment_path: &Path) -> std::io::Result<()> {
    match fs::remove_file(rollup_path(segment_path)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// What a sidecar must agree with to be believed: identity of the segment it
/// claims to describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) segment_bytes: u64,
    pub(crate) record_count: u64,
    pub(crate) min_start_ns: u64,
    pub(crate) max_start_ns: u64,
}

/// Reads the sidecar for `segment_path`, or `None` if there is nothing
/// trustworthy there.
///
/// Every rejection is silent and returns `None`, because every rejection has
/// the same correct remedy — rebuild from the segment — and because this runs
/// on the query path, where a hard error over a DERIVED file would take down
/// a request that the segment itself can still answer.
pub(crate) fn load(segment_path: &Path, expected: Binding) -> Option<SegmentRollup> {
    let bytes = fs::read(rollup_path(segment_path)).ok()?;
    decode(&bytes, expected)
}

/// The timestamp ranges of `segment_path`'s rollup, from a single
/// [`PROLOGUE_LEN`]-byte read.
///
/// This is the question TTL expiry asks about every segment on every tick:
/// *could this segment hold anything expirable?* Answering it by decoding the
/// segment costs a JSON parse per span and made the sweep re-read the whole
/// corpus once a minute whether or not anything was expirable. Answering it
/// from the sidecar's prologue costs one short read.
///
/// The prologue carries its own checksum, so this never acts on unverified
/// bytes — which matters, because the caller deletes data on the strength of
/// the answer. `None` means "no trustworthy answer", and every caller must
/// fall back to reading the segment rather than assuming either way.
pub(crate) fn bounds(segment_path: &Path, expected: Binding) -> Option<Bounds> {
    use std::io::Read;
    let mut file = fs::File::open(rollup_path(segment_path)).ok()?;
    let mut head = [0u8; PROLOGUE_LEN];
    file.read_exact(&mut head).ok()?;
    if head[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut cursor = Cursor {
        bytes: &head,
        at: MAGIC.len(),
    };
    if cursor.u16()? != FORMAT_VERSION {
        return None;
    }
    cursor.u16()?; // reserved
    if cursor.u32()? != SCHEMA_VERSION {
        return None;
    }
    let found = Binding {
        segment_bytes: cursor.u64()?,
        record_count: cursor.u64()?,
        min_start_ns: cursor.u64()?,
        max_start_ns: cursor.u64()?,
    };
    let bounds = Bounds {
        min_start_ns: found.min_start_ns,
        max_start_ns: found.max_start_ns,
        min_end_ns: cursor.u64()?,
        max_end_ns: cursor.u64()?,
    };
    if cursor.u64()? != fnv1a(&head[..PROLOGUE_CHECKSUM_AT]) {
        return None;
    }
    // Checked LAST, so a mismatched binding is distinguished from corruption
    // only by which check failed — both answer `None`, which is the same
    // instruction to the caller: go and read the segment.
    if found != expected {
        return None;
    }
    Some(bounds)
}

/// Writes the sidecar for `segment_path`, atomically.
///
/// Temp file plus rename, so a reader sees a whole sidecar or no sidecar and
/// never a half-written one. The bytes are NOT fsynced: this is a derived
/// cache, and a crash that loses it costs one rebuild, so paying a sync per
/// seal to protect it would be spending durability budget on something that
/// is never the authority for any span.
pub(crate) fn store(
    segment_path: &Path,
    binding: Binding,
    rollup: &SegmentRollup,
) -> std::io::Result<()> {
    let final_path = rollup_path(segment_path);
    let directory = final_path.parent().unwrap_or(Path::new("."));
    let file_name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = directory.join(format!(".{file_name}.{}.{counter}.tmp", std::process::id()));

    let result = (|| {
        // Refuse to publish a sidecar for a segment that is no longer the one
        // it describes.
        //
        // `Store::segment_rollup` writes a sidecar after rebuilding one, which
        // is what heals a store that never had them. But a rebuild reads a
        // segment it does not own: TTL expiry rewrites a segment IN PLACE,
        // under the same name, and writes the correct sidecar as it goes. A
        // reader that started before that rewrite finishes holds a rollup for
        // the pre-expiry bytes, and without this check it would rename that
        // stale rollup over the fresh one. The binding would reject it on the
        // next read, so no wrong answer is served — but the segment would then
        // be decoded in full to rebuild what expiry had already written, which
        // is the whole cost this file exists to avoid.
        //
        // Checked immediately before the rename, so the window is a metadata
        // read wide. Losing the race the other way costs one stale sidecar
        // that the binding rejects — the same outcome as no sidecar at all.
        if fs::metadata(segment_path)?.len() != binding.segment_bytes {
            return Ok(());
        }
        let encoded = encode(binding, rollup);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp_path)?;
        file.write_all(&encoded)?;
        drop(file);
        fs::rename(&temp_path, &final_path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

// --------------------------------------------------------------- encoding

fn encode(binding: Binding, rollup: &SegmentRollup) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096 + rollup.key_hashes.len() * 8);
    out.extend_from_slice(&MAGIC);
    put_u16(&mut out, FORMAT_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, SCHEMA_VERSION);
    put_u64(&mut out, binding.segment_bytes);
    put_u64(&mut out, binding.record_count);
    put_u64(&mut out, binding.min_start_ns);
    put_u64(&mut out, binding.max_start_ns);
    let bounds = rollup.bounds();
    put_u64(&mut out, bounds.min_end_ns);
    put_u64(&mut out, bounds.max_end_ns);
    debug_assert_eq!(out.len(), PROLOGUE_CHECKSUM_AT);
    let prologue_checksum = fnv1a(&out);
    put_u64(&mut out, prologue_checksum);
    debug_assert_eq!(out.len(), PROLOGUE_LEN);

    // Maps are written in SORTED key order so that encoding the same rollup
    // twice produces identical bytes, the same determinism rule the segment
    // encoder holds itself to.
    put_counter_map(&mut out, &rollup.by_model);
    put_counter_map(&mut out, &rollup.by_provider);
    put_counter_map(&mut out, &rollup.by_service);
    put_counter_map(&mut out, &rollup.by_day);
    put_counter_map(&mut out, &rollup.by_session_key);

    let mut sessions: Vec<(&String, &SessionCounters)> = rollup.sessions.iter().collect();
    sessions.sort_by(|left, right| left.0.cmp(right.0));
    put_u32(&mut out, sessions.len() as u32);
    for (id, session) in sessions {
        put_str(&mut out, id);
        put_counters(&mut out, &session.counters);
        put_u64(&mut out, session.first_start_ns);
        put_u64(&mut out, session.last_end_ns);
        let mut traces: Vec<&String> = session.traces.iter().collect();
        traces.sort();
        put_u32(&mut out, traces.len() as u32);
        for trace in traces {
            put_str(&mut out, trace);
        }
        put_u32(&mut out, session_key_id(session.session_key));
    }

    let mut hashes: Vec<u64> = rollup.key_hashes.iter().copied().collect();
    hashes.sort_unstable();
    put_u64(&mut out, hashes.len() as u64);
    for hash in hashes {
        put_u64(&mut out, hash);
    }

    let mut refs: Vec<&String> = rollup.payload_refs.iter().collect();
    refs.sort();
    put_u32(&mut out, refs.len() as u32);
    for reference in refs {
        put_str(&mut out, reference);
    }

    let checksum = fnv1a(&out);
    put_u64(&mut out, checksum);
    out
}

fn decode(bytes: &[u8], expected: Binding) -> Option<SegmentRollup> {
    // The checksum covers everything before it, so verifying it FIRST means
    // every read below is over bytes already known to be intact — the
    // decoder never has to defend itself against garbage, only against a
    // truncated file, which the length checks handle.
    if bytes.len() < MAGIC.len() + 8 {
        return None;
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 8);
    if fnv1a(body) != u64::from_le_bytes(trailer.try_into().ok()?) {
        return None;
    }

    let mut cursor = Cursor { bytes: body, at: 0 };
    if cursor.take(MAGIC.len())? != MAGIC {
        return None;
    }
    if cursor.u16()? != FORMAT_VERSION {
        return None;
    }
    cursor.u16()?; // reserved
    if cursor.u32()? != SCHEMA_VERSION {
        return None;
    }
    let found = Binding {
        segment_bytes: cursor.u64()?,
        record_count: cursor.u64()?,
        min_start_ns: cursor.u64()?,
        max_start_ns: cursor.u64()?,
    };
    // The sidecar describes some segment; this proves it describes THIS one.
    if found != expected {
        return None;
    }
    let bounds = Bounds {
        min_start_ns: found.min_start_ns,
        max_start_ns: found.max_start_ns,
        min_end_ns: cursor.u64()?,
        max_end_ns: cursor.u64()?,
    };
    // The prologue carries its own checksum so `bounds` can be read without
    // the rest of the file; verified here too, so the two readers cannot
    // disagree about the same bytes.
    if cursor.u64()? != fnv1a(&body[..PROLOGUE_CHECKSUM_AT]) {
        return None;
    }

    let mut rollup = SegmentRollup::empty(bounds);
    rollup.by_model = cursor.counter_map()?;
    rollup.by_provider = cursor.counter_map()?;
    rollup.by_service = cursor.counter_map()?;
    rollup.by_day = cursor.counter_map()?.into_iter().collect();
    rollup.by_session_key = cursor.counter_map()?;

    let session_count = cursor.u32()? as usize;
    rollup.sessions.reserve(session_count);
    for _ in 0..session_count {
        let id = cursor.string()?;
        let counters = cursor.counters()?;
        let first_start_ns = cursor.u64()?;
        let last_end_ns = cursor.u64()?;
        let trace_count = cursor.u32()? as usize;
        let mut traces = HashSet::with_capacity(trace_count);
        for _ in 0..trace_count {
            traces.insert(cursor.string()?);
        }
        let session_key = session_key_name(cursor.u32()?)?;
        rollup.sessions.insert(
            id,
            SessionCounters {
                counters,
                first_start_ns,
                last_end_ns,
                traces,
                session_key,
            },
        );
    }

    let hash_count = usize::try_from(cursor.u64()?).ok()?;
    let mut key_hashes = HashSet::with_capacity(hash_count);
    for _ in 0..hash_count {
        key_hashes.insert(cursor.u64()?);
    }
    rollup.key_hashes = key_hashes;

    let ref_count = cursor.u32()? as usize;
    let mut payload_refs = HashSet::with_capacity(ref_count);
    for _ in 0..ref_count {
        payload_refs.insert(cursor.string()?);
    }
    rollup.payload_refs = payload_refs;

    // Trailing bytes mean the writer and reader disagree about the layout,
    // which makes everything decoded above suspect however well it parsed.
    if cursor.at != body.len() {
        return None;
    }
    Some(rollup)
}

/// Index of a recognized session key, or `u32::MAX` for none.
fn session_key_id(key: Option<&'static str>) -> u32 {
    key.and_then(|key| semconv::SESSION_KEYS.iter().position(|known| *known == key))
        .map_or(u32::MAX, |index| index as u32)
}

/// Inverse of [`session_key_id`]. An out-of-range index means the recognized
/// key list changed without [`SCHEMA_VERSION`] being bumped; the outer
/// `Option` rejects the whole sidecar rather than guessing a key, because a
/// guessed key silently regroups sessions.
fn session_key_name(id: u32) -> Option<Option<&'static str>> {
    if id == u32::MAX {
        return Some(None);
    }
    semconv::SESSION_KEYS.get(id as usize).map(|key| Some(*key))
}

// ------------------------------------------------------------- primitives

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn put_counters(out: &mut Vec<u8>, counters: &Counters) {
    put_u64(out, counters.spans as u64);
    put_u64(out, counters.llm_calls as u64);
    put_u64(out, counters.prompt_tokens);
    put_u64(out, counters.completion_tokens);
    put_u64(out, counters.total_tokens);
    // Cost is carried as its IEEE-754 bit pattern, not as text: a decimal
    // round-trip of an accumulated f64 is not guaranteed to reproduce the
    // same value, and this file exists to return exactly what a rebuild
    // would have returned.
    put_u64(out, counters.cost_usd.to_bits());
    put_u64(out, counters.errors as u64);
    put_u64(out, counters.llm_duration_ns);
}

fn put_counter_map<M>(out: &mut Vec<u8>, map: &M)
where
    for<'a> &'a M: IntoIterator<Item = (&'a String, &'a Counters)>,
{
    let mut entries: Vec<(&String, &Counters)> = map.into_iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    put_u32(out, entries.len() as u32);
    for (key, counters) in entries {
        put_str(out, key);
        put_counters(out, counters);
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn counters(&mut self) -> Option<Counters> {
        Some(Counters {
            spans: usize::try_from(self.u64()?).ok()?,
            llm_calls: usize::try_from(self.u64()?).ok()?,
            prompt_tokens: self.u64()?,
            completion_tokens: self.u64()?,
            total_tokens: self.u64()?,
            cost_usd: f64::from_bits(self.u64()?),
            errors: usize::try_from(self.u64()?).ok()?,
            llm_duration_ns: self.u64()?,
        })
    }

    fn counter_map(&mut self) -> Option<HashMap<String, Counters>> {
        let count = self.u32()? as usize;
        let mut map = HashMap::with_capacity(count);
        for _ in 0..count {
            let key = self.string()?;
            map.insert(key, self.counters()?);
        }
        Some(map)
    }
}

/// FNV-1a, the same construction `analytics::key_hash` uses. A checksum here
/// only has to catch truncation and bit rot; it is not a security boundary.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
