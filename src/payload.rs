//! Content-addressed payload offloading for oversized span text.
//!
//! Multi-megabyte prompts and completions do not belong inside segment
//! records: they bloat every decode that touches the span. Above a
//! configured threshold, string attribute values are extracted to
//! `payloads/<aa>/<sha256>.bin` under the data directory and replaced in
//! the span by a reference object:
//!
//! ```json
//! {"$payload": "sha256/<hex>", "bytes": 123456, "preview": "first chars…"}
//! ```
//!
//! Content addressing dedupes identical payloads — an agent's system prompt
//! repeated across ten thousand calls is stored once. Files are immutable
//! once written; the TTL compactor deletes payload files older than the
//! retention window (an orphan from an unflushed span lingers at most one
//! TTL). SHA-256 is implemented here (FIPS 180-4), dependency-free, and
//! verified against the standard test vectors in the module tests.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::{Error, Result, Span};

/// Key marking a payload reference object.
pub const PAYLOAD_KEY: &str = "$payload";
/// Characters of original text kept inline as a preview.
const PREVIEW_CHARS: usize = 256;
/// How long a freshly touched payload is immune from the TTL sweep. The
/// compactor snapshots live references, releases the locks, then deletes;
/// an ingest can commit a NEW reference to an OLD file (content-address
/// dedup does not refresh mtime) inside that window. The store is
/// single-process (DirectoryLock), so an in-memory touch registry is
/// complete knowledge: anything touched within this window cannot be swept.
const TOUCH_IMMUNITY: Duration = Duration::from_secs(600);

/// The in-process registry of recently written or deduplicated payload
/// references, keyed by `sha256/<hex>`.
pub(crate) type TouchRegistry = Mutex<HashMap<String, Instant>>;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ----------------------------------------------------------------- sha-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `bytes` as lowercase hex (FIPS 180-4).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut schedule = [0_u32; 64];
    for block in message.chunks_exact(64) {
        for (i, word) in schedule.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = schedule[i - 15].rotate_right(7)
                ^ schedule[i - 15].rotate_right(18)
                ^ (schedule[i - 15] >> 3);
            let s1 = schedule[i - 2].rotate_right(17)
                ^ schedule[i - 2].rotate_right(19)
                ^ (schedule[i - 2] >> 10);
            schedule[i] = schedule[i - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(schedule[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
    let mut out = String::with_capacity(64);
    for word in state {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

// ------------------------------------------------------------ payload store

/// Directory name under the data dir.
pub(crate) const PAYLOAD_DIR: &str = "payloads";

pub(crate) fn payload_path(directory: &Path, hash: &str) -> PathBuf {
    let shard = hash.get(0..2).unwrap_or("00");
    directory
        .join(PAYLOAD_DIR)
        .join(shard)
        .join(format!("{hash}.bin"))
}

/// Writes `content` to the content-addressed store (idempotent — an existing
/// file with the same hash is left alone) and returns the reference object.
/// The touch is registered BEFORE any filesystem work, so a concurrent sweep
/// can never observe the file without its immunity.
pub(crate) fn store_payload(
    directory: &Path,
    content: &str,
    registry: &TouchRegistry,
) -> Result<Value> {
    let hash = sha256_hex(content.as_bytes());
    registry
        .lock()
        .map_err(|_| Error::LockPoisoned("payload registry"))?
        .insert(format!("sha256/{hash}"), Instant::now());
    let path = payload_path(directory, &hash);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write via a WRITER-UNIQUE temp + rename: a shared `<hash>.tmp`
        // let ten concurrent identical ingests truncate each other's temp
        // and race the rename (found in review: 9 successes, one ENOENT).
        // Unique temps make every rename valid; identical content means
        // whichever rename lands last is byte-identical anyway.
        let temp = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        {
            let mut file = fs::File::create(&temp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temp, &path)?;
    }
    let preview: String = content.chars().take(PREVIEW_CHARS).collect();
    let mut reference = Map::new();
    reference.insert(PAYLOAD_KEY.into(), Value::String(format!("sha256/{hash}")));
    reference.insert("bytes".into(), Value::from(content.len()));
    reference.insert("preview".into(), Value::String(preview));
    Ok(Value::Object(reference))
}

/// Reads a payload by its `sha256/<hex>` reference. `None` when absent.
pub(crate) fn load_payload(directory: &Path, reference: &str) -> Result<Option<Vec<u8>>> {
    let Some(hash) = reference.strip_prefix("sha256/") else {
        return Ok(None);
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    // The hash is validated hex, so the path cannot traverse.
    match fs::read(payload_path(directory, hash)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Replaces every string attribute value longer than `threshold` bytes
/// (span attributes and event attributes) with a payload reference.
pub(crate) fn offload_span(
    directory: &Path,
    span: &mut Span,
    threshold: usize,
    registry: &TouchRegistry,
) -> Result<()> {
    offload_map(directory, &mut span.attributes, threshold, registry)?;
    for event in &mut span.events {
        offload_map(directory, &mut event.attributes, threshold, registry)?;
    }
    Ok(())
}

fn offload_map(
    directory: &Path,
    attributes: &mut Map<String, Value>,
    threshold: usize,
    registry: &TouchRegistry,
) -> Result<()> {
    for value in attributes.values_mut() {
        if let Value::String(text) = value {
            if text.len() > threshold {
                *value = store_payload(directory, text, registry)?;
            }
        }
    }
    Ok(())
}

/// Drops touch-immunity entries that have aged out.
///
/// Split out of [`sweep_expired`] because it is the registry's ONLY pruner,
/// and the sweep is now skipped entirely for a store with no payload
/// directory. Leaving it inside would have made "this store has no payloads"
/// mean "this store's touch registry grows without bound".
pub(crate) fn prune_touch_registry(registry: &TouchRegistry) -> Result<()> {
    registry
        .lock()
        .map_err(|_| Error::LockPoisoned("payload registry"))?
        .retain(|_, at| at.elapsed() < TOUCH_IMMUNITY);
    Ok(())
}

/// Deletes payload files that are BOTH older than the cutoff and no longer
/// referenced by any live span. Age alone is not grounds for deletion:
/// content addressing means a fresh span can re-reference an old file
/// (identical content dedupes to one path without refreshing its mtime),
/// and deleting by mtime alone destroyed exactly that live data (found in
/// review, reproduced). Returns the number of files removed.
pub(crate) fn sweep_expired(
    directory: &Path,
    cutoff: std::time::SystemTime,
    live: &HashSet<String>,
    registry: &TouchRegistry,
) -> Result<usize> {
    // Prune stale immunity entries briefly, then traverse without the
    // registry lock: a large payload directory must not stall every ingest.
    prune_touch_registry(registry)?;
    let root = directory.join(PAYLOAD_DIR);
    if !root.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for shard in fs::read_dir(&root)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let modified = metadata.modified().unwrap_or(cutoff);
            let reference = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|hash| format!("sha256/{hash}"))
                .unwrap_or_default();
            if modified < cutoff && !live.contains(&reference) {
                // Serialize only the final touch check and deletion with
                // store_payload's pre-filesystem registration. If ingest
                // touches first, this skips the file; if deletion wins,
                // ingest observes the missing path and recreates it before
                // committing its span.
                let touched = registry
                    .lock()
                    .map_err(|_| Error::LockPoisoned("payload registry"))?;
                if !touched.contains_key(&reference) {
                    fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::{sha256_hex, store_payload, sweep_expired, TouchRegistry};
    use std::collections::HashSet;
    use std::time::{Duration, Instant, SystemTime};

    #[test]
    fn recently_touched_payloads_are_immune_from_the_sweep() {
        // The compactor snapshots live refs, RELEASES the locks, then
        // sweeps: a ref committed inside that window must survive even
        // though the stale snapshot does not contain it.
        let dir = std::env::temp_dir().join(format!(
            "traza-touch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let registry = TouchRegistry::default();
        let reference = store_payload(&dir, "fresh-touch", &registry).expect("stores");
        let reference = reference["$payload"].as_str().expect("ref").to_owned();

        // Sweep with an EMPTY live set and a future cutoff (the file's
        // mtime is in the past relative to it): only the touch protects it.
        let cutoff = SystemTime::now() + Duration::from_secs(3_600);
        let removed = sweep_expired(&dir, cutoff, &HashSet::new(), &registry).expect("sweeps");
        assert_eq!(removed, 0, "a freshly touched payload must survive");

        // Age the touch beyond the immunity window: now it is sweepable.
        registry
            .lock()
            .expect("registry")
            .insert(reference, Instant::now() - Duration::from_secs(700));
        let removed = sweep_expired(&dir, cutoff, &HashSet::new(), &registry).expect("sweeps");
        assert_eq!(removed, 1, "an old, unreferenced, untouched payload sweeps");
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        // FIPS 180-4 / NIST test vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&million_a),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
