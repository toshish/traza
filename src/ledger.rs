//! The ledger: a hash chain over what this store has committed.
//!
//! A generation's manifest proves what the store holds NOW — every
//! load-bearing file, digested. It cannot prove what the store used to hold,
//! or that today's manifest descends from yesterday's: each checkpoint writes
//! a fresh manifest, and whoever controls the disk can rewrite the whole set
//! consistently. The ledger is the missing history. Every published
//! generation, every published erasure, and every external countersignature
//! becomes one line of `ledger.jsonl`, and each line's `head` commits to the
//! line before it:
//!
//! ```text
//! head = sha256("traza-ledger/1\n" + prev + "\n" + canonical_json(entry without "head"))
//! ```
//!
//! `prev` is the previous entry's head (64 zeros for the genesis entry), and
//! canonical JSON is the crate's one canonical form (`evals::canonical_json`):
//! keys sorted bytewise, no whitespace, serde_json's string escaping and
//! number formatting. Entries carry only strings, integers and `null`, so the
//! formula is reproducible with any JSON library that parses integers exactly
//! (timestamps exceed 2^53, so a doubles-only parser such as JavaScript's
//! `JSON.parse` does not) — `docs/guide/ledger.md` has a ten-line script, and
//! that reproducibility is the point: an auditor must not need this code to
//! check this code's claims.
//!
//! **Erasure entries are salted.** Every erasure entry carries a random
//! `salt` inside its head's preimage. A tenant's evidence pack shows other
//! tenants' erasure entries as stubs (`seq`, `prev`, `head`), and without the
//! salt every other input to such a head is guessable — the erasure id is a
//! small integer, the subject kind one of five, and a tenant subject's digest
//! depends only on the tenant's name — so a tenant could recompute stub heads
//! for candidate neighbours and learn which one was erased. With the salt
//! withheld from the stub, it cannot; with the salt on the full entry, an
//! operator or auditor who knows the subject still recomputes everything.
//!
//! **What the chain proves, and what it does not.** Rewriting any committed
//! entry changes its head and breaks every later `prev`; recomputing the
//! whole tail to hide that is easy, because there is no secret here. What
//! makes a rewrite DETECTABLE is an anchor: an external notary's
//! countersignature over a head, stored back into the chain as an `anchor`
//! entry and kept by the notary. A history rewritten after an anchor no longer
//! reproduces the anchored head. The ledger cannot prove anything about
//! generations compacted away before an anchor, about files altered between
//! two checkpoints (the next checkpoint digests what it finds), or about data
//! that was never sent. `docs/guide/ledger.md` states the threat model.
//!
//! **Outside generations, on purpose.** The ledger commits TO generations, so
//! it cannot live inside one — a manifest that digested the ledger would have
//! to digest the entry describing itself. It sits at the store root, is never
//! manifested, and a pin carries a copy of it (`pins/<label>/ledger.jsonl`)
//! as evidence of the history up to that pin. A restore therefore does not
//! install a backup's ledger: the restored store keeps the ledger already in
//! its data directory, or starts a new chain (new `store_id`) in an empty one,
//! and either way ledgers the installed generation at its first open.
//!
//! **Evidence, not state.** A damaged ledger never stops the store: open heals
//! a torn final append exactly as the other append-only logs do, tolerates
//! anything else, and keeps appending from the last entry it can read.
//! Verification is what reports a break, naming the entry. A ledger append
//! that FAILS (a full disk, a file the server may not write) never fails what
//! it records either: the checkpoint, erasure or open it belongs to has
//! already committed, and reporting a committed generation as a failure would
//! leave an erasure unsettled — its admission barrier dropping the tenant's
//! new spans — over evidence. The failure is kept instead, reported by
//! verification until the next checkpoint re-ledgers what it missed. The one
//! refusal is a ledger open cannot READ: without the tip, an append would
//! fork the chain rather than extend it.
//!
//! **Crash consistency.** Entries are appended after the fact they record is
//! durable — a checkpoint entry after `CURRENT` moved, an erasure entry after
//! its settle record — so a crash between the two leaves a fact without an
//! entry, never an entry without a fact. Open reconciles that gap: it ledgers
//! the live generation when the latest checkpoint entry names a different
//! manifest, and every settled erasure the chain does not yet carry. The same
//! reconcile is what ledgers generations published while no store was open —
//! a restore's install, a format migration's checkpoint, and the store's
//! first generation.
//!
//! **One snapshot per answer.** Every reader — a page, a verification, an
//! evidence pack, a pin's copy — reads a prefix of the file whose length was
//! taken under the tip lock, never the file to its end: erasure and anchor
//! appends are not serialized by the maintenance lock a verification takes,
//! and reading to EOF could take in half a line mid-append. The same fixed,
//! never-rewritten prefix is what lets a verification walk the chain after
//! releasing that lock, and lets an evidence pack extend an earlier walk
//! instead of starting again at byte 0.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{erasure, evals, generation, payload, Error, Result, SpanFilter, Store};

/// File name of the ledger, at the store root beside `CURRENT`.
pub(crate) const LOG_NAME: &str = "ledger.jsonl";

/// Domain-separation prefix of every head. The version names the formula, so
/// a future change to it is a new prefix rather than a silent reinterpretation
/// of old heads.
pub const HEAD_DOMAIN: &str = "traza-ledger/1\n";

/// The `prev` of the genesis entry: 64 zeros, the head of nothing.
pub const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The evidence pack's format identifier.
pub const EVIDENCE_FORMAT: &str = "traza-evidence/1";

/// Ceiling on one page of `GET /v1/ledger`.
pub const MAX_PAGE: usize = 1000;

/// Page size when `GET /v1/ledger` names no limit.
const DEFAULT_PAGE: usize = 100;

/// Ceiling on each anchor string field. Anchors are stored verbatim and
/// opaque, so this is what keeps one request from writing an arbitrarily wide
/// line into a file every verification reads whole.
const MAX_ANCHOR_FIELD_BYTES: usize = 4096;

/// Ceiling on the ledger entries one evidence pack carries. A year of the
/// server's five-minute checkpoints is about 105,000 entries; a pack past this
/// says so (`entries_truncated`) and the rest pages through `GET /v1/ledger`.
/// Lowered under test so the cap itself is exercised.
#[cfg(not(test))]
const EVIDENCE_MAX_ENTRIES: usize = 10_000;
#[cfg(test)]
const EVIDENCE_MAX_ENTRIES: usize = 4;

/// Ceiling on the erasure receipts one evidence pack runs. Each receipt is a
/// real verification walk, so a window with many erasures would otherwise
/// make one request cost many walks; past this, `conclusive` is `null` and
/// `GET /v1/erasures/{id}/verify` answers for that erasure. Lowered under
/// test, likewise.
#[cfg(not(test))]
const EVIDENCE_MAX_RECEIPTS: usize = 32;
#[cfg(test)]
const EVIDENCE_MAX_RECEIPTS: usize = 1;

/// Ceiling on the erasures one evidence pack lists. A pack past it says so
/// (`erasures_truncated`); `GET /v1/erasures` lists them all. Lowered under
/// test so the cap itself is exercised.
#[cfg(not(test))]
const EVIDENCE_MAX_ERASURES: usize = 10_000;
#[cfg(test)]
const EVIDENCE_MAX_ERASURES: usize = 4;

/// Chain lines between deadline checks in a budgeted walk: the crate's
/// usual interval, a line costing about what a decode does. Lowered under
/// test so that a walk cut short at its first check is a short one.
#[cfg(not(test))]
const WALK_CHECK_INTERVAL: u64 = crate::DEADLINE_CHECK_INTERVAL as u64;
#[cfg(test)]
const WALK_CHECK_INTERVAL: u64 = 64;

/// How old a COMPLETED check may be before evidence packs start redoing it:
/// the chain walk from byte 0, and each manifested file's digest.
/// Re-digesting every manifested byte is O(store) and re-walking the chain
/// O(ledger); a pack is readable by bound (tenant) credentials, and without
/// reuse each of their requests would buy both. With it, a pack extends the
/// last completed walk by what was appended since, and reuses each file's
/// last digest while its manifest entry is unchanged.
///
/// It bounds completed results only. An UNFINISHED walk or digest is never
/// discarded for its age — what it established is about a fixed prefix of
/// an append-only file, or a file it holds open — or a check longer than
/// this window, or retried across it, could never finish. And a completed
/// result past the window is still served, with its age stated
/// (`chain_verified_at_ns`, `verified_at_ns`), while packs redo it in the
/// background of their own budgets ([`EVIDENCE_REFRESH_SLICE`]); that is
/// what keeps a pack's checks recent: an edit to the disk under a running
/// server shows in packs once the redo reaches it. `GET /v1/ledger/verify`
/// never reuses.
const EVIDENCE_VERIFY_REUSE: Duration = Duration::from_secs(60);

/// The most a pack spends redoing checks past [`EVIDENCE_VERIFY_REUSE`],
/// after the pack itself is assembled: whatever its budget has left, and
/// never more than this, so a stale check is refreshed a slice per pack
/// without any pack paying for the whole of it. Each slice makes some
/// progress even when the budget is spent (one check interval), so the
/// redo always advances.
const EVIDENCE_REFRESH_SLICE: Duration = Duration::from_millis(250);

/// Manifested bytes a pack's file check digests between budget checks, a
/// file opened counting as [`FILE_OPEN_COST`] of them; and manifest bytes a
/// [`ManifestLoad`] reads between them. Lowered under test, so a small
/// store's check is cut short more than once.
#[cfg(not(test))]
const FILE_CHECK_BYTES: u64 = 8 * 1024 * 1024;
#[cfg(test)]
const FILE_CHECK_BYTES: u64 = 64 * 1024;

/// What opening a file costs a pack's file check, in digested bytes, so a
/// store of many small files still reaches its budget checks.
const FILE_OPEN_COST: u64 = 16 * 1024;

/// Settle records a pack takes in from the tombstone log per lock hold and
/// budget check.
const SETTLE_CHUNK: usize = WALK_CHECK_INTERVAL as usize;

/// Ceiling on the problems of each kind an evidence pack's verification
/// names — damage to the chain, ledgered erasures the tombstone log no
/// longer holds, settled erasures the chain lacks, failing files. Past it
/// the rest of that kind is one line with their count, and
/// `problems_truncated` says so. Lowered under test so the cap itself is
/// exercised.
#[cfg(not(test))]
const EVIDENCE_MAX_PROBLEMS: usize = 100;
#[cfg(test)]
const EVIDENCE_MAX_PROBLEMS: usize = 4;

/// How many times one pack runs its steps from a new snapshot because the
/// store moved under its file check — published another generation, or
/// replaced a file whose problem it was confirming (see [`FileVerifier`]).
/// Each round continues from what the last kept, so a second is the most
/// an ordinary checkpoint costs; this bounds a store publishing faster than
/// a pack can look, budget or none.
const EVIDENCE_MAX_ROUNDS: u32 = 8;

/// Provisional file problems a pack confirms per hold of the maintenance
/// lock — one metadata read each — with a budget check between holds (see
/// [`FileVerifier::confirm`]). Lowered under test so a few problems take
/// several holds.
#[cfg(not(test))]
const CONFIRM_BATCH: usize = 256;
#[cfg(test)]
const CONFIRM_BATCH: usize = 16;

/// Files' states a replaced file check frees between budget checks (see
/// [`Evidence::reclaim`]).
const RECLAIM_CHUNK: usize = WALK_CHECK_INTERVAL as usize;

// ------------------------------------------------------------------ entries

/// What one entry records, by `kind`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// The first entry, written at the ledger's first open.
    Genesis {
        /// 32 random hex characters naming this chain. A store restored into
        /// an empty directory starts a new chain and so gets a new id.
        store_id: String,
    },
    /// A published generation whose manifest differs from the last one
    /// ledgered.
    Checkpoint {
        /// The generation id `CURRENT` names.
        generation: u64,
        /// SHA-256 of `generations/<id>/state-manifest.json` exactly as it sits
        /// on disk — the bytes a pin copies, so `sha256sum` reproduces it.
        manifest_sha256: String,
    },
    /// A published (settled) erasure. Never the subject itself: the digest
    /// lets someone who already knows the subject confirm its erasure was
    /// recorded, without the ledger disclosing it to anyone else.
    Erasure {
        /// The tombstone log's id for the erasure.
        erasure_id: u64,
        /// The tenant the subject targets (empty is the default tenant), or
        /// `None` for a payload subject, which is store-global.
        tenant: Option<String>,
        /// `trace`, `span`, `session`, `payload`, or `tenant`.
        subject_kind: String,
        /// SHA-256 of the canonical JSON of the subject as the tombstone log
        /// records it (see [`subject_digest`]).
        subject_digest: String,
        /// The generation whose publication carried the deletion — the
        /// settle record's `generation`. It is also what tells two
        /// publications of the same erasure id apart: a restore of an
        /// earlier pin rewinds the tombstone log, so re-applying an erasure
        /// afterwards can reuse an id and a subject the chain already
        /// carries, but never a generation (a data directory never reuses
        /// one; an install takes the next id past both stores).
        generation: u64,
        /// 32 random hex characters inside the head's preimage, so a stub
        /// of this entry (in another tenant's evidence pack) cannot be
        /// matched to a guessed subject by recomputing its head. See the
        /// module docs.
        salt: String,
    },
    /// An external notary's countersignature over an earlier head, stored
    /// opaque: Traza neither checks the signature nor knows the scheme.
    Anchor {
        /// The entry whose head was signed.
        target_seq: u64,
        /// That entry's head, as the notary saw it.
        target_head: String,
        /// Who signed: a notary name, a URL, a key fingerprint.
        authority: String,
        /// When the notary says it signed, nanoseconds since the Unix epoch.
        anchored_at_ns: u64,
        /// The signature, in whatever encoding the notary uses.
        signature: String,
        /// The key that verifies it, likewise.
        public_key: String,
        /// Free text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl Body {
    /// The wire name of this entry's kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Genesis { .. } => "genesis",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Erasure { .. } => "erasure",
            Self::Anchor { .. } => "anchor",
        }
    }
}

/// One ledger entry: the common fields plus its kind's body, flattened into
/// one JSON object per line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Position in the chain, from 0.
    pub seq: u64,
    /// Wall-clock append time, nanoseconds since the Unix epoch.
    pub at_ns: u64,
    /// The previous entry's head; [`GENESIS_PREV`] for seq 0.
    pub prev: String,
    /// This entry's head; see the module docs for the formula.
    pub head: String,
    /// What the entry records.
    #[serde(flatten)]
    pub body: Body,
}

/// The chain's tip, as `GET /v1/ledger/head` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LedgerHead {
    /// The tip's seq.
    pub seq: u64,
    /// The tip's head.
    pub head: String,
    /// The genesis entry's store id, or `None` when no readable genesis
    /// entry exists (a damaged ledger; verification names it).
    pub store_id: Option<String>,
    /// The tip's append time.
    pub at_ns: u64,
}

/// An anchor as a notary submits it: the anchor body minus everything the
/// ledger assigns (`seq`, `prev`, `head`, `at_ns`). Unknown fields are
/// refused, so a caller cannot smuggle chain fields in.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchorRequest {
    /// The entry whose head was signed.
    pub target_seq: u64,
    /// That entry's head; must equal the ledger's head at `target_seq`.
    pub target_head: String,
    /// Who signed.
    pub authority: String,
    /// When the notary signed, nanoseconds since the Unix epoch.
    pub anchored_at_ns: u64,
    /// The signature, opaque.
    pub signature: String,
    /// The verifying key, opaque.
    pub public_key: String,
    /// Free text.
    #[serde(default)]
    pub note: Option<String>,
}

/// The head of an entry: `sha256(HEAD_DOMAIN + prev + "\n" +
/// canonical_json(entry without "head"))`, lowercase hex.
///
/// `entry` may carry a `head` field; it is excluded. `prev` is passed
/// separately because it is what the formula chains on, and is ALSO inside
/// the hashed object as a field — the two must agree for a chain to verify.
pub fn compute_head(prev: &str, entry: &Value) -> String {
    let mut unsigned = entry.clone();
    if let Value::Object(map) = &mut unsigned {
        map.remove("head");
    }
    let mut preimage = String::with_capacity(HEAD_DOMAIN.len() + prev.len() + 256);
    preimage.push_str(HEAD_DOMAIN);
    preimage.push_str(prev);
    preimage.push('\n');
    preimage.push_str(&evals::canonical_json(&unsigned));
    payload::sha256_hex(preimage.as_bytes())
}

/// SHA-256 of the canonical JSON of an erasure subject, as the tombstone log
/// records it — `{"kind":"session","session_id":"…","tenant":"acme"}`, with
/// `tenant` omitted for the default tenant. Someone who knows the subject can
/// recompute this; nobody else learns the subject from it, although a
/// GUESSABLE subject (a short or sequential id) can be confirmed by guessing.
pub fn subject_digest(subject: &erasure::Subject) -> String {
    let value = serde_json::to_value(subject).unwrap_or(Value::Null);
    payload::sha256_hex(evals::canonical_json(&value).as_bytes())
}

/// The wire name of a subject's kind.
fn subject_kind(subject: &erasure::Subject) -> &'static str {
    match subject {
        erasure::Subject::Trace { .. } => "trace",
        erasure::Subject::Span { .. } => "span",
        erasure::Subject::Session { .. } => "session",
        erasure::Subject::Payload { .. } => "payload",
        erasure::Subject::Tenant { .. } => "tenant",
    }
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA-256 of a generation's manifest file, exactly as stored.
fn manifest_digest(directory: &Path, generation: u64) -> Result<String> {
    let bytes = fs::read(generation::manifest_path(directory, generation))?;
    Ok(payload::sha256_hex(&bytes))
}

/// 32 random hex characters, for a chain's `store_id` and an erasure entry's
/// `salt`.
///
/// OS randomness where the platform exposes it as a file, always mixed with
/// two outputs of std's randomly keyed hasher (its keys come from the OS
/// generator on every platform std supports), the clock, the pid, a
/// process-wide counter and `context`. The salt's job needs the result
/// unpredictable, which either source alone provides; the store id's only
/// needs it distinct. No dependency is needed for either.
fn random_hex(context: &[u8]) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = Vec::with_capacity(96);
    if let Ok(mut file) = File::open("/dev/urandom") {
        let mut buffer = [0u8; 32];
        if file.read_exact(&mut buffer).is_ok() {
            seed.extend_from_slice(&buffer);
        }
    }
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = crate::unix_now_ns();
    for lane in 0_u8..2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u8(lane);
        hasher.write(context);
        hasher.write_u64(count);
        hasher.write_u64(now);
        seed.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    seed.extend_from_slice(&now.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(&count.to_le_bytes());
    payload::sha256_hex(&seed)[..32].to_owned()
}

/// A new chain's id. Unique, not secret: two stores must not share one, and
/// nothing is authenticated by it.
fn fresh_store_id(directory: &Path) -> String {
    random_hex(directory.as_os_str().as_encoded_bytes())
}

// --------------------------------------------------------------------- log

/// The ledger file plus what appending needs to know about its tip.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    /// Leaf-level: taken briefly, never while acquiring another engine lock.
    /// Held across an append's write and fsync, which is what serializes
    /// appends and gives every reader a length that ends on a whole line.
    tip: Mutex<Tip>,
    /// What evidence packs keep between requests — the chain walks, the
    /// erasure cross-check, the settled erasures taken in, the file check —
    /// so each pack pays for what changed; see [`Store::evidence`]. The one
    /// ledger lock that is NOT leaf-level: a pack takes it BEFORE the
    /// maintenance lock and holds it across its work, which runs after the
    /// maintenance lock is released (and takes that lock again only briefly,
    /// to confirm a file problem). So packs work one at a time, each from
    /// where the last stopped, every snapshot is taken after the walk it
    /// extends was stored (a stored walk never runs past a later snapshot),
    /// and no pack holds a checkpoint off while it walks or digests. Nothing
    /// takes it while holding another engine lock.
    evidence: Mutex<Evidence>,
    /// Appends that failed since the ledger last caught up. Leaf-level.
    failures: Mutex<Failures>,
    /// Test-only pauses at points a race would otherwise have to be hit at.
    #[cfg(test)]
    hooks: Mutex<Hooks>,
    /// Test-only: how far this ledger's clock runs ahead of the real one.
    #[cfg(test)]
    clock: Mutex<Duration>,
}

/// The mark [`Ledger::settling`] puts up, taken down on drop.
pub(crate) struct Settling<'a> {
    ledger: &'a Ledger,
    id: u64,
    marked: bool,
}

impl Drop for Settling<'_> {
    fn drop(&mut self) {
        if !self.marked {
            return;
        }
        if let Ok(mut tip) = self.ledger.lock() {
            if let Some(count) = tip.settling.get_mut(&self.id) {
                *count -= 1;
                if *count == 0 {
                    tip.settling.remove(&self.id);
                }
            }
        }
    }
}

/// A test hook: runs once, at its point, with the store.
#[cfg(test)]
type Hook = Box<dyn FnOnce(&Store) + Send>;

/// Where a test hook runs.
#[cfg(test)]
#[derive(Clone, Copy)]
enum HookPoint {
    /// In `complete_erasure`, between the settle record and the entry.
    AfterSettle,
    /// In a live verification, just after its snapshot was taken.
    AfterSnapshot,
}

#[cfg(test)]
#[derive(Default)]
struct Hooks {
    after_settle: Option<Hook>,
    after_snapshot: Option<Hook>,
}

#[cfg(test)]
impl std::fmt::Debug for Hooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Hooks")
    }
}

#[derive(Debug, Default)]
struct Tip {
    /// The last readable entry, or `None` for an empty ledger.
    last: Option<(u64, String, u64)>,
    store_id: Option<String>,
    /// `manifest_sha256` of the latest checkpoint entry.
    last_manifest: Option<String>,
    /// `(generation, length)`: the latest checkpoint entry's generation and
    /// the ledger's length in bytes just past that entry's line — the prefix
    /// a pin of that generation copies.
    last_checkpoint_end: Option<(u64, u64)>,
    /// The bytes of the file the chain owns: through the end of the last
    /// line appended or read. Anything past it is a torn or failed append
    /// that healing could not remove, and the next append cuts it off first.
    len: u64,
    /// The final entry is complete but lacks its newline, because adding one
    /// at open failed: the next append writes it first.
    unterminated: bool,
    /// Every erasure entry — one publication of one erasure, keyed by
    /// `(erasure_id, subject_digest, generation)` — with the byte offset
    /// just past its line. The digest guards against an id reused by
    /// another store's lineage (a restore of its backup into this
    /// directory); the generation tells a re-application after a rollback
    /// restore from the publication already ledgered; the offset tells a
    /// live verification whether the entry is inside the prefix it read or
    /// landed after it.
    erasures: HashMap<Publication, u64>,
    /// The erasure entries this process appended, in append order — so by
    /// ascending offset — each as `(end, erasure_id, generation,
    /// subject_digest)`, `end` being the offset just past its line. A live
    /// verification finds the ones past its prefix with one binary search
    /// instead of looking up every settled publication its walk lacks (see
    /// [`Ledger::landing`]). Entries read at open are not here: they all end
    /// within the first snapshot any verification can take.
    appended: Vec<(u64, u64, u64, Key)>,
    /// Erasures between their settle record and their entry, with how many
    /// completions of each are in that window; see [`Ledger::settling`].
    settling: HashMap<u64, u32>,
}

/// One publication of one erasure, as the ledger keys it:
/// `(erasure_id, subject_digest, generation)`.
type Publication = (u64, String, u64);

impl Tip {
    /// Takes in one entry whose line ends at byte `end` of the file.
    fn absorb(&mut self, entry: &Entry, end: u64) {
        self.last = Some((entry.seq, entry.head.clone(), entry.at_ns));
        match &entry.body {
            Body::Genesis { store_id } => {
                if self.store_id.is_none() {
                    self.store_id = Some(store_id.clone());
                }
            }
            Body::Checkpoint {
                generation,
                manifest_sha256,
            } => {
                self.last_manifest = Some(manifest_sha256.clone());
                self.last_checkpoint_end = Some((*generation, end));
            }
            Body::Erasure {
                erasure_id,
                subject_digest,
                generation,
                ..
            } => {
                self.erasures
                    .entry((*erasure_id, subject_digest.clone(), *generation))
                    .or_insert(end);
            }
            Body::Anchor { .. } => {}
        }
    }

    fn head(&self) -> LedgerHead {
        let (seq, head, at_ns) = self.last.clone().unwrap_or((0, GENESIS_PREV.to_owned(), 0));
        LedgerHead {
            seq,
            head,
            store_id: self.store_id.clone(),
            at_ns,
        }
    }
}

/// A consistent view of the chain: its tip, and the length of the file
/// prefix that ends on it. Readers read exactly that prefix.
#[derive(Clone, Debug)]
struct Snapshot {
    head: LedgerHead,
    len: u64,
}

/// Append failures not yet caught up: what they would have recorded is not
/// in the chain yet. `epoch` counts failures ever noted, so a catch-up clears
/// only the failures it began after — one landing mid-catch-up stays
/// reported.
#[derive(Debug, Default)]
struct Failures {
    epoch: u64,
    outstanding: Option<Failure>,
}

#[derive(Clone, Debug)]
struct Failure {
    count: u64,
    first_at_ns: u64,
    last_error: String,
}

/// One file verification result, as a verification reports it.
#[derive(Clone, Debug)]
struct FileCheck {
    /// When the oldest digest it rests on began.
    at_ns: u64,
    /// The failing files' problems: every one for the operator's check, at
    /// most [`EVIDENCE_MAX_PROBLEMS`] for a pack's.
    problems: Vec<String>,
    /// How many files fail, itemized or not.
    failing: usize,
}

impl Ledger {
    /// Opens the ledger, healing a torn final append, and writes the genesis
    /// entry when the ledger holds no readable entry.
    ///
    /// Only an UNTERMINATED final line that does not parse is healed — the
    /// one shape a crash mid-append produces, since every append writes its
    /// line and newline in one write. Anything else that fails to parse is
    /// left exactly as found: it is evidence of damage, and verification
    /// names it.
    ///
    /// Fails only when the file cannot be READ. A heal or genesis that
    /// cannot be WRITTEN is noted as an append failure (verification reports
    /// it) and retried by the next append, which cuts off whatever the heal
    /// could not and writes the genesis first.
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut tip = Tip::default();
        let mut heal_failure = None;
        if path.exists() {
            let mut reader = BufReader::new(File::open(&path)?);
            let mut valid_len = 0_u64;
            let mut line = Vec::new();
            let mut torn = false;
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                let terminated = line.ends_with(b"\n");
                match serde_json::from_slice::<Entry>(&line) {
                    Ok(entry) => {
                        valid_len += read as u64;
                        tip.absorb(&entry, valid_len);
                        tip.unterminated = !terminated;
                    }
                    Err(_) if !terminated => {
                        torn = true;
                        break;
                    }
                    Err(_) => valid_len += read as u64,
                }
            }
            tip.len = valid_len;
            let healed = if torn {
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.set_len(valid_len).and_then(|()| file.sync_all()))
            } else if tip.unterminated {
                // A complete entry missing only its newline: terminate it,
                // or the next append would be glued onto the same line.
                OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .and_then(|mut file| file.write_all(b"\n").and_then(|()| file.sync_all()))
                    .map(|()| {
                        tip.unterminated = false;
                        tip.len += 1;
                        if let Some((_, end)) = &mut tip.last_checkpoint_end {
                            if *end == valid_len {
                                *end += 1;
                            }
                        }
                    })
            } else {
                Ok(())
            };
            if let Err(error) = healed {
                heal_failure = Some(Error::Io(error));
            }
        }
        let ledger = Self {
            path,
            tip: Mutex::new(tip),
            evidence: Mutex::new(Evidence::default()),
            failures: Mutex::new(Failures::default()),
            #[cfg(test)]
            hooks: Mutex::new(Hooks::default()),
            #[cfg(test)]
            clock: Mutex::new(Duration::ZERO),
        };
        if let Some(error) = heal_failure {
            ledger.note_failure("healing the final line at open", &error);
        }
        let genesis = {
            let mut tip = ledger.lock()?;
            match tip.last.is_none() {
                true => {
                    let store_id = fresh_store_id(directory);
                    ledger
                        .append_locked(&mut tip, Body::Genesis { store_id })
                        .map(drop)
                }
                false => Ok(()),
            }
        };
        if let Err(error) = genesis {
            ledger.note_failure("the genesis entry", &error);
        }
        Ok(ledger)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Tip>> {
        self.tip.lock().map_err(|_| Error::LockPoisoned("ledger"))
    }

    /// The clock reuse windows are judged by: the real one, run ahead under
    /// test so a window can be crossed without waiting it out.
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Ok(offset) = self.clock.lock() {
            return Instant::now() + *offset;
        }
        Instant::now()
    }

    /// Wall-clock nanoseconds on the same clock as [`Self::now`].
    fn unix_now_ns(&self) -> u64 {
        #[cfg(test)]
        if let Ok(offset) = self.clock.lock() {
            return crate::unix_now_ns().saturating_add(offset.as_nanos() as u64);
        }
        crate::unix_now_ns()
    }

    /// Runs this ledger's clock `by` further ahead.
    #[cfg(test)]
    fn advance_clock(&self, by: Duration) {
        if let Ok(mut offset) = self.clock.lock() {
            *offset += by;
        }
    }

    fn lock_failures(&self) -> Result<std::sync::MutexGuard<'_, Failures>> {
        self.failures
            .lock()
            .map_err(|_| Error::LockPoisoned("ledger failures"))
    }

    /// Keeps an append failure for verification to report. Never fails: a
    /// poisoned lock here would mean a panic mid-note, and the failure is
    /// then reported by the chain itself (the missing entry).
    fn note_failure(&self, what: &str, error: &Error) {
        if let Ok(mut failures) = self.lock_failures() {
            failures.epoch = failures.epoch.wrapping_add(1);
            let failure = failures.outstanding.get_or_insert_with(|| Failure {
                count: 0,
                first_at_ns: crate::unix_now_ns(),
                last_error: String::new(),
            });
            failure.count = failure.count.saturating_add(1);
            failure.last_error = format!("{what}: {error}");
        }
    }

    /// The failure count so far, for a catch-up to clear only what it began
    /// after; see [`Failures`].
    fn failure_epoch(&self) -> Option<u64> {
        self.lock_failures().ok().map(|failures| failures.epoch)
    }

    /// Clears the outstanding failure if none was noted since `epoch`.
    fn clear_failures_since(&self, epoch: Option<u64>) {
        if let (Some(epoch), Ok(mut failures)) = (epoch, self.lock_failures()) {
            if failures.epoch == epoch {
                failures.outstanding = None;
            }
        }
    }

    fn outstanding_failure(&self) -> Option<Failure> {
        self.lock_failures()
            .ok()
            .and_then(|failures| failures.outstanding.clone())
    }

    /// The tip and the prefix length that ends on it, read together.
    fn snapshot(&self) -> Result<Snapshot> {
        let tip = self.lock()?;
        Ok(Snapshot {
            head: tip.head(),
            len: tip.len,
        })
    }

    /// Appends `body` as the next entry, durably, under the held tip lock —
    /// after a genesis entry first, when the chain has none yet (one that
    /// failed at open is retried here, so a chain never starts headless).
    fn append_locked(&self, tip: &mut Tip, body: Body) -> Result<Entry> {
        if tip.last.is_none() && !matches!(body, Body::Genesis { .. }) {
            let store_id = fresh_store_id(self.path.parent().unwrap_or(Path::new(".")));
            self.append_one(tip, Body::Genesis { store_id })?;
        }
        self.append_one(tip, body)
    }

    fn append_one(&self, tip: &mut Tip, body: Body) -> Result<Entry> {
        let (seq, prev) = match &tip.last {
            Some((seq, head, _)) => (seq.saturating_add(1), head.clone()),
            None => (0, GENESIS_PREV.to_owned()),
        };
        let mut entry = Entry {
            seq,
            at_ns: crate::unix_now_ns(),
            prev,
            head: String::new(),
            body,
        };
        entry.head = compute_head(&entry.prev, &serde_json::to_value(&entry)?);
        let mut line = String::new();
        if tip.unterminated {
            line.push('\n');
        }
        line.push_str(&serde_json::to_string(&entry)?);
        line.push('\n');
        let created = !self.path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // The file must be exactly the chain's bytes before a line goes on:
        // anything past them is a torn or failed append that healing could
        // not remove, and glued onto, it would read as a broken chain
        // forever after.
        if file.metadata()?.len() > tip.len {
            file.set_len(tip.len)?;
        }
        if let Err(error) = file
            .write_all(line.as_bytes())
            .and_then(|()| file.sync_all())
        {
            // Best effort: if this truncate fails too, the next append's cut
            // above is the backstop, and no reader looks past `tip.len`.
            let _ = file.set_len(tip.len);
            return Err(Error::Io(error));
        }
        // The line is in the file: the tip moves now, even if the directory
        // sync below fails, or the next append would reuse this seq.
        tip.len += line.len() as u64;
        tip.unterminated = false;
        tip.absorb(&entry, tip.len);
        if let Body::Erasure {
            erasure_id,
            subject_digest,
            generation,
            ..
        } = &entry.body
        {
            let end = tip.len;
            tip.appended
                .push((end, *erasure_id, *generation, hex_key(subject_digest)));
        }
        if created {
            if let Some(directory) = self.path.parent() {
                crate::sync_directory(directory)?;
            }
        }
        Ok(entry)
    }

    /// Ledgers a published generation unless its manifest is the one the
    /// latest checkpoint entry already names.
    fn record_checkpoint(&self, generation: u64, manifest_sha256: String) -> Result<Option<Entry>> {
        let mut tip = self.lock()?;
        if tip.last_manifest.as_deref() == Some(manifest_sha256.as_str()) {
            return Ok(None);
        }
        self.append_locked(
            &mut tip,
            Body::Checkpoint {
                generation,
                manifest_sha256,
            },
        )
        .map(Some)
    }

    /// Ledgers one publication of an erasure — published by `generation` —
    /// unless the chain already carries that publication.
    fn record_erasure(
        &self,
        erasure_id: u64,
        subject: &erasure::Subject,
        generation: u64,
    ) -> Result<Option<Entry>> {
        let digest = subject_digest(subject);
        let mut tip = self.lock()?;
        if tip
            .erasures
            .contains_key(&(erasure_id, digest.clone(), generation))
        {
            return Ok(None);
        }
        self.append_locked(
            &mut tip,
            Body::Erasure {
                erasure_id,
                tenant: subject.tenant().map(str::to_owned),
                subject_kind: subject_kind(subject).to_owned(),
                subject_digest: digest,
                generation,
                salt: random_hex(b"erasure salt"),
            },
        )
        .map(Some)
    }

    /// Ledgers every settled erasure in `statuses` the chain does not yet
    /// carry. Idempotent; stops at the first failed append.
    fn reconcile_erasures(&self, statuses: &[erasure::ErasureStatus]) -> Result<()> {
        for status in statuses {
            if let Some(settle) = &status.settle {
                self.record_erasure(status.erase.id, &status.erase.subject, settle.generation)?;
            }
        }
        Ok(())
    }

    /// Marks erasure `id` as settling until the guard drops. Taken before
    /// its settle record is written and dropped after its entry's append
    /// returned, so for the whole window in which the tombstone log can
    /// hold the settle while the chain lacks the entry, a live verification
    /// can tell an entry on its way from a missing one.
    ///
    /// Never fails: the erasure must settle whatever the ledger does. With
    /// the tip lock poisoned nothing is marked, and the verification that
    /// would consult the mark fails on the same lock.
    fn settling(&self, id: u64) -> Settling<'_> {
        let marked = match self.lock() {
            Ok(mut tip) => {
                *tip.settling.entry(id).or_insert(0) += 1;
                true
            }
            Err(_) => false,
        };
        Settling {
            ledger: self,
            id,
            marked,
        }
    }

    /// What a verification of the snapshot whose prefix is `length` bytes
    /// must not call missing, of the settled publications its walk lacks:
    /// the erasure entries appended past that prefix, and the erasures still
    /// being appended (marked [`Self::settling`]). [`CrossCheck::landing`]
    /// keeps the ones its walk lacks.
    ///
    /// Read under one tip lock, and after the settlements, which is what
    /// makes it exact: an erasure whose settle the read saw had its mark up
    /// before the settle was written, and drops the mark only after its
    /// append, so here — however much later — it is either still marked or
    /// its entry is past the prefix (a failed append leaves neither, and
    /// stays reported). Both are small whatever the store holds — appends
    /// since the snapshot, and erasures settling now — so this costs neither
    /// the chain nor the publications a damaged store's walk lacks.
    fn landing(&self, length: u64) -> Result<Landing> {
        let tip = self.lock()?;
        let past = tip.appended.partition_point(|&(end, ..)| end <= length);
        Ok(Landing {
            appended: tip.appended[past..]
                .iter()
                .map(|&(_, id, generation, digest)| (id, generation, digest))
                .collect(),
            settling: tip.settling.keys().copied().collect(),
        })
    }

    /// Runs the test hook installed for this point, if any.
    #[cfg(test)]
    fn run_hook(&self, point: HookPoint, store: &Store) {
        let hook = self.hooks.lock().ok().and_then(|mut hooks| match point {
            HookPoint::AfterSettle => hooks.after_settle.take(),
            HookPoint::AfterSnapshot => hooks.after_snapshot.take(),
        });
        if let Some(hook) = hook {
            hook(store);
        }
    }

    /// Appends an anchor after checking it names a real head.
    fn anchor(&self, request: AnchorRequest) -> Result<Entry> {
        let fields = [
            ("authority", &request.authority),
            ("signature", &request.signature),
            ("public_key", &request.public_key),
        ];
        for (name, value) in fields {
            if value.is_empty() {
                return Err(Error::Invalid(format!("anchor {name} is empty")));
            }
            if value.len() > MAX_ANCHOR_FIELD_BYTES {
                return Err(Error::Invalid(format!(
                    "anchor {name} is {} bytes, over the {MAX_ANCHOR_FIELD_BYTES}-byte limit",
                    value.len()
                )));
            }
        }
        if request
            .note
            .as_ref()
            .is_some_and(|note| note.len() > MAX_ANCHOR_FIELD_BYTES)
        {
            return Err(Error::Invalid(format!(
                "anchor note is over the {MAX_ANCHOR_FIELD_BYTES}-byte limit"
            )));
        }
        // A malformed head cannot equal the recorded one, and the contract
        // answers every target_head that is not the recorded head with a
        // conflict — so this is a 409 too, not a 400, and it does not echo
        // the value back.
        if !is_sha256_hex(&request.target_head) {
            return Err(Error::Conflict(format!(
                "target_head is not 64 lowercase hex characters, so it cannot be the head \
                 recorded at seq {}",
                request.target_seq
            )));
        }
        // The check and the append share one tip lock: an append landing
        // between them cannot change what `target_seq` names (entries are
        // never rewritten), but it would race the anchor's own seq.
        let mut tip = self.lock()?;
        let Some((tip_seq, tip_head, _)) = tip.last.clone() else {
            return Err(Error::Conflict("the ledger has no entries".to_owned()));
        };
        if request.target_seq > tip_seq {
            return Err(Error::Conflict(format!(
                "the ledger has no entry {} yet; its head is seq {tip_seq}",
                request.target_seq
            )));
        }
        let recorded = match request.target_seq == tip_seq {
            true => Some(tip_head),
            false => self.head_at(request.target_seq, tip.len)?,
        };
        match recorded {
            Some(head) if head == request.target_head => {}
            Some(head) => {
                return Err(Error::Conflict(format!(
                    "target_head does not match the head at seq {}: the ledger records {head}",
                    request.target_seq
                )))
            }
            None => {
                return Err(Error::Conflict(format!(
                    "no readable entry at seq {}; verify the ledger",
                    request.target_seq
                )))
            }
        }
        self.append_locked(
            &mut tip,
            Body::Anchor {
                target_seq: request.target_seq,
                target_head: request.target_head,
                authority: request.authority,
                anchored_at_ns: request.anchored_at_ns,
                signature: request.signature,
                public_key: request.public_key,
                note: request.note,
            },
        )
    }

    /// The recorded head of the first readable entry carrying `seq`, within
    /// the first `length` bytes.
    fn head_at(&self, seq: u64, length: u64) -> Result<Option<String>> {
        let mut found = None;
        self.scan_prefix(length, |entry| {
            if entry.get("seq").and_then(Value::as_u64) == Some(seq) {
                found = entry.get("head").and_then(Value::as_str).map(str::to_owned);
                return false;
            }
            true
        })?;
        Ok(found)
    }

    /// Streams every line of the first `length` bytes to `visit` — the JSON
    /// object it holds, or `None` for a non-blank line that is not one —
    /// until it returns false. Blank lines are skipped.
    ///
    /// Runs under no lock: `length` comes from a [`Snapshot`] (or the held
    /// tip), the file is append-only, and nothing ever rewrites bytes inside
    /// the chain's prefix, so a long read never holds up an append.
    fn scan_lines(
        &self,
        length: u64,
        mut visit: impl FnMut(Option<Map<String, Value>>) -> bool,
    ) -> Result<()> {
        if length == 0 {
            return Ok(());
        }
        let mut reader = BufReader::new(File::open(&self.path)?.take(length));
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            #[cfg(test)]
            probe::parsed_one();
            let entry = match serde_json::from_slice::<Value>(&line) {
                Ok(Value::Object(map)) => Some(map),
                _ => None,
            };
            if !visit(entry) {
                return Ok(());
            }
        }
    }

    /// [`Self::scan_lines`], readable entries only.
    fn scan_prefix(&self, length: u64, mut visit: impl FnMut(Value) -> bool) -> Result<()> {
        self.scan_lines(length, |entry| match entry {
            Some(map) => visit(Value::Object(map)),
            None => true,
        })
    }

    fn head(&self) -> Result<LedgerHead> {
        Ok(self.lock()?.head())
    }

    /// Copies the chain through the checkpoint entry for `generation` to
    /// `target`, when that is the latest checkpoint entry, and returns
    /// whether it did. Returns false when the latest checkpoint names
    /// another generation — its own append failed — since a copy that did
    /// not end on the pinned checkpoint would misstate what it evidences.
    ///
    /// The prefix length is taken under the tip lock and copied outside it:
    /// an erasure or anchor appended between the checkpoint and the copy
    /// lands past that length, so the copy ends exactly on the pinned
    /// checkpoint, whatever raced it.
    fn copy_through_checkpoint(&self, generation: u64, target: &Path) -> Result<bool> {
        let end = match self.lock()?.last_checkpoint_end {
            Some((ledgered, end)) if ledgered == generation => end,
            _ => return Ok(false),
        };
        generation::copy_prefix(&self.path, target, end)?;
        Ok(true)
    }

    fn lock_evidence(&self) -> Result<std::sync::MutexGuard<'_, Evidence>> {
        self.evidence
            .lock()
            .map_err(|_| Error::LockPoisoned("ledger evidence state"))
    }

    /// Offers a complete walk from byte 0, begun at `started`, for evidence
    /// packs to extend: kept when it is fresher than the one they serve,
    /// and then a redo begun before it is dropped, since finishing it could
    /// only give an older answer. Its length was a snapshot's, so it cannot
    /// run past a later snapshot. Its cross-check is rebuilt by the next
    /// pack, from memory: the walk keeps every erasure entry it read.
    fn offer_walk(&self, walk: ChainWalk, started: Instant, started_at_ns: u64) -> Result<()> {
        if walk.missing {
            return Ok(());
        }
        let mut evidence = self.lock_evidence()?;
        // Packs answer for a pinned snapshot until one does, and a walk past
        // it cannot serve that shorter prefix: they keep the walk they have.
        if evidence
            .pin
            .as_ref()
            .is_some_and(|pin| pin.snapshot.len < walk.len)
        {
            return Ok(());
        }
        if evidence
            .served
            .as_ref()
            .map_or(true, |held| held.started < started)
        {
            let target = walk.len;
            evidence.served = Some(CachedWalk {
                walk,
                cross: CrossCheck::default(),
                started,
                started_at_ns,
                target,
            });
            if evidence
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.started < started)
            {
                evidence.refresh = None;
            }
        }
        Ok(())
    }

    /// The JSON object on each line that starts at one of `offsets` (in
    /// ascending order; every one a line a walk placed, so inside the
    /// chain's prefix and ending in a newline). Reads those lines and no
    /// others.
    fn read_lines_at(&self, offsets: &[u64]) -> Result<Vec<Value>> {
        if offsets.is_empty() {
            return Ok(Vec::new());
        }
        let mut reader = BufReader::new(File::open(&self.path)?);
        let mut position = 0_u64;
        let mut line = Vec::new();
        let mut out = Vec::with_capacity(offsets.len());
        for &offset in offsets {
            // Contiguous lines — the usual window — read straight on; a gap
            // costs one seek.
            if offset != position {
                reader.seek(SeekFrom::Start(offset))?;
            }
            line.clear();
            position = offset + reader.read_until(b'\n', &mut line)? as u64;
            #[cfg(test)]
            probe::parsed_one();
            if let Ok(value @ Value::Object(_)) = serde_json::from_slice::<Value>(&line) {
                out.push(value);
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------ verification

/// Test-only accounting of the chain work a request does, on the thread
/// that does it: lines of the ledger parsed, and a hook at the start of
/// each chain walk. Thread-local because every reader runs on its caller's
/// thread, which keeps parallel tests from counting each other's work.
#[cfg(test)]
mod probe {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static PARSED: Cell<u64> = const { Cell::new(0) };
        static DIGESTED: Cell<u64> = const { Cell::new(0) };
        static EXAMINED: Cell<u64> = const { Cell::new(0) };
        static HASHED: Cell<u64> = const { Cell::new(0) };
        static ALLOWANCE: Cell<Option<u64>> = const { Cell::new(None) };
        static ON_WALK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static ON_HASH: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static MANIFESTED: Cell<u64> = const { Cell::new(0) };
        static ON_MANIFEST: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static CONFIRMED: Cell<u64> = const { Cell::new(0) };
        static FREED: Cell<u64> = const { Cell::new(0) };
        static FREED_UNDER_MAINTENANCE: Cell<u64> = const { Cell::new(0) };
        static HOLDING_MAINTENANCE: Cell<bool> = const { Cell::new(false) };
    }

    /// Counts one provisional file problem looked at again to confirm it.
    pub(super) fn confirmed_one() {
        CONFIRMED.with(|confirmed| confirmed.set(confirmed.get() + 1));
    }

    /// Provisional file problems looked at again on this thread so far.
    pub(super) fn confirmations() -> u64 {
        CONFIRMED.with(Cell::get)
    }

    /// Counts one file's state in a pack's file check freed, and whether a
    /// pack on this thread held the maintenance lock as it was.
    pub(super) fn freed_file() {
        FREED.with(|freed| freed.set(freed.get() + 1));
        if HOLDING_MAINTENANCE.with(Cell::get) {
            FREED_UNDER_MAINTENANCE.with(|freed| freed.set(freed.get() + 1));
        }
    }

    /// Files' states freed on this thread so far: in all, and while a pack
    /// held the maintenance lock.
    pub(super) fn freed_files() -> (u64, u64) {
        (
            FREED.with(Cell::get),
            FREED_UNDER_MAINTENANCE.with(Cell::get),
        )
    }

    /// Marks this thread as holding the maintenance lock for a pack until
    /// the returned guard drops.
    pub(super) fn holding_maintenance() -> Holding {
        HOLDING_MAINTENANCE.with(|held| held.set(true));
        Holding
    }

    pub(super) struct Holding;

    impl Drop for Holding {
        fn drop(&mut self) {
            HOLDING_MAINTENANCE.with(|held| held.set(false));
        }
    }

    /// Counts one manifest entry a pack's file check took in.
    pub(super) fn manifest_entry() {
        MANIFESTED.with(|taken| taken.set(taken.get() + 1));
    }

    /// Manifest entries taken in on this thread so far.
    pub(super) fn manifest_entries() -> u64 {
        MANIFESTED.with(Cell::get)
    }

    /// Runs the hook set by [`on_manifest`], once.
    pub(super) fn reading_manifest() {
        if let Some(hook) = ON_MANIFEST.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }

    /// Sets a hook for the next read of a manifest's bytes by a pack on this
    /// thread.
    pub(super) fn on_manifest(hook: Box<dyn FnOnce()>) {
        ON_MANIFEST.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    /// Counts one ledger line parsed.
    pub(super) fn parsed_one() {
        PARSED.with(|parsed| parsed.set(parsed.get() + 1));
    }

    /// Ledger lines parsed on this thread so far.
    pub(super) fn parsed() -> u64 {
        PARSED.with(Cell::get)
    }

    /// Counts one settled erasure's subject digested for the cross-check.
    pub(super) fn digested_one() {
        DIGESTED.with(|digested| digested.set(digested.get() + 1));
    }

    /// Settled erasures digested on this thread so far.
    pub(super) fn digested() -> u64 {
        DIGESTED.with(Cell::get)
    }

    /// Counts one index entry looked at to find a pack's window — of the
    /// chain or of the settled erasures — or one cross-check step.
    pub(super) fn examined_one() {
        EXAMINED.with(|examined| examined.set(examined.get() + 1));
    }

    /// Index entries looked at on this thread so far.
    pub(super) fn examined() -> u64 {
        EXAMINED.with(Cell::get)
    }

    /// Counts `bytes` of manifested files digested by a pack's file check.
    pub(super) fn hashed(bytes: u64) {
        HASHED.with(|hashed| hashed.set(hashed.get() + bytes));
    }

    /// Manifested bytes a pack's file check digested on this thread so far.
    pub(super) fn hashed_bytes() -> u64 {
        HASHED.with(Cell::get)
    }

    /// Lets the next `checks` budget checks on this thread pass whatever
    /// the deadline says, and fails every one after them; `None` lifts the
    /// limit. A budget that runs out at a chosen point, without a clock.
    pub(super) fn allow(checks: Option<u64>) {
        ALLOWANCE.with(|allowance| allowance.set(checks));
    }

    /// Spends one budget check: true when the allowance is used up.
    pub(super) fn spent() -> bool {
        ALLOWANCE.with(|allowance| match allowance.get() {
            None => false,
            Some(0) => true,
            Some(left) => {
                allowance.set(Some(left - 1));
                false
            }
        })
    }

    /// Runs the hook set by [`on_walk`], once.
    pub(super) fn walking() {
        if let Some(hook) = ON_WALK.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }

    /// Sets a hook for the next chain walk on this thread.
    pub(super) fn on_walk(hook: Box<dyn FnOnce()>) {
        ON_WALK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    /// Runs the hook set by [`on_hash`], once.
    pub(super) fn hashing() {
        if let Some(hook) = ON_HASH.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }

    /// Sets a hook for the next manifested file a pack's check digests on
    /// this thread.
    pub(super) fn on_hash(hook: Box<dyn FnOnce()>) {
        ON_HASH.with(|slot| *slot.borrow_mut() = Some(hook));
    }
}

/// Whether a budgeted step must stop here: its deadline has passed — or,
/// under test, the thread's allowance of checks is spent (see
/// `probe::allow`). Without a deadline, never: an unbudgeted step (the
/// operator's verification, a store with no query budget) is never cut
/// short.
fn out_of_budget(deadline: Option<crate::Deadline>) -> bool {
    if deadline.is_none() {
        return false;
    }
    #[cfg(test)]
    if probe::spent() {
        return true;
    }
    crate::Deadline::check(deadline, 0).is_err()
}

/// The result of verifying the ledger and the live generation against it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verification {
    /// True when every head recomputes and links, the latest checkpoint names
    /// the live generation's manifest, every manifested file verifies, every
    /// ledgered erasure still holds in the live tombstone log, every settled
    /// erasure there is ledgered, and no ledger append is outstanding. The
    /// store's verdict, in a tenant's evidence pack as anywhere else.
    pub intact: bool,
    /// Entry lines in the ledger, readable or not.
    pub entries: u64,
    /// The recorded head of the last readable entry.
    pub head: Option<String>,
    /// When the walk this verification rests on began at the chain's first
    /// entry, nanoseconds since the Unix epoch: the age of its oldest
    /// finding. An evidence pack extends an earlier walk by what was
    /// appended since, so this can be well before the pack; a fresh
    /// verification's is the moment it ran.
    pub chain_verified_at_ns: u64,
    /// Every discrepancy, each naming the entry (`seq N`) or file it is about.
    /// An evidence pack names at most [`EVIDENCE_MAX_PROBLEMS`] of each kind
    /// and counts the rest in a line after them; a tenant's pack also
    /// reduces file problems, and withholds other tenants', to a count.
    pub problems: Vec<String>,
    /// True when `problems` counts some problem its reader may see without
    /// naming it: an evidence pack past [`EVIDENCE_MAX_PROBLEMS`] of one
    /// kind. Problems withheld from a tenant are not counted here — they
    /// were never the tenant's to see. Always false for
    /// `GET /v1/ledger/verify` and the offline check, which name every
    /// problem.
    pub problems_truncated: bool,
    /// The live generation, checked against the chain and against its files.
    pub generation: GenerationCheck,
}

/// How much of what a verification found it names, and for whom.
#[derive(Clone, Copy, Debug)]
enum Detail<'a> {
    /// Every problem, by name: the operator's verification, live or
    /// offline, whose cost is O(store) by design.
    All,
    /// An evidence pack's, for `reader` — a tenant, or `None` for the whole
    /// store. At most [`EVIDENCE_MAX_PROBLEMS`] of each kind are named, the
    /// rest counted; for a tenant, file problems (which name store-global
    /// files) are one count, and problems about another tenant's erasures,
    /// or a store-global payload erasure, another that says nothing of
    /// whose. Bound credentials can ask for packs as often as they like, so
    /// a pack's cost may grow with what it names, never with everything the
    /// store has wrong.
    Pack(Option<&'a str>),
}

/// The live generation's half of a [`Verification`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationCheck {
    /// The generation `CURRENT` names.
    pub id: u64,
    /// SHA-256 of its manifest file.
    pub manifest_sha256: String,
    /// Whether the latest checkpoint entry names this generation and digest.
    pub matches_ledger: bool,
    /// Whether every file the manifest names re-digests to its recorded
    /// digest (the same check as `GET /v1/verify`).
    pub files_verified: bool,
    /// When the file check ran, nanoseconds since the Unix epoch — for an
    /// evidence pack, which re-digests files a budget at a time, when the
    /// OLDEST file digest it rests on began.
    pub verified_at_ns: u64,
}

impl Verification {
    /// The human-readable report `traza-server verify --ledger` prints.
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "ledger: {} entries, head {}\n\
             generation {}: manifest {} · matches ledger: {} · files verified: {}\n",
            self.entries,
            self.head.as_deref().unwrap_or("(none)"),
            self.generation.id,
            self.generation.manifest_sha256,
            yes_no(self.generation.matches_ledger),
            yes_no(self.generation.files_verified),
        );
        for problem in &self.problems {
            out.push_str(&format!("  problem: {problem}\n"));
        }
        out.push_str(match self.intact {
            true => "result: intact\n",
            false => "result: BROKEN\n",
        });
        out
    }
}

fn yes_no(value: bool) -> &'static str {
    match value {
        true => "yes",
        false => "NO",
    }
}

/// A walk of the chain that can be resumed: everything checking the first
/// `len` bytes established, so checking a longer prefix starts where this
/// one stopped instead of at byte 0.
///
/// Sound because the file is append-only and nothing the store does
/// rewrites bytes inside the chain's prefix: what a walk established about
/// a prefix stays established, unless the disk is edited under a running
/// server. That is why evidence packs redo a completed walk from byte 0 once
/// it is older than [`EVIDENCE_VERIFY_REUSE`], and the operator's
/// verification always walks from byte 0. It is also why an UNFINISHED walk
/// is never thrown away for its age: nothing it established has expired.
///
/// Works on the generic JSON of each line, never a decoded [`Entry`]: a field
/// added by hand must change the recomputed head, and decoding into a typed
/// struct would silently drop it. Problems name the entry by `seq` where the
/// line says one, else by line number.
#[derive(Clone, Debug)]
struct ChainWalk {
    /// Bytes walked: the end of the last line taken in, so the next line
    /// starts here — or, when that line lacked its newline
    /// ([`Self::unterminated`]), one byte before it does.
    len: u64,
    /// The last line taken in ended at the walk's target without a newline:
    /// a final entry the open could not terminate. The next append writes
    /// that newline first, and the next [`Self::advance`] steps over it
    /// rather than read it as a blank line of its own.
    unterminated: bool,
    line_number: u64,
    /// Entry lines walked, readable or not (blank lines excluded).
    entries: u64,
    /// The recorded head of the last readable entry.
    head: Option<String>,
    /// `(seq, generation, manifest_sha256)` of the last checkpoint entry.
    last_checkpoint: Option<(u64, u64, String)>,
    /// Every erasure entry, in chain order, for the cross-check against the
    /// live tombstone log. Every one, not the latest per subject: each is
    /// its own publication, and an older one cannot vouch for a newer one
    /// of the same subject (see [`assemble`]).
    erasures: Vec<LedgeredErasure>,
    /// Problems found so far; [`Self::problems`] adds the ones only the
    /// whole walk can state.
    found: Vec<String>,
    /// What the next entry must say, or `None` after a line that could not
    /// be read — the chain cannot be followed across it, and reporting every
    /// later entry as broken would bury the one real finding.
    expected: Option<(u64, String)>,
    /// Recorded heads by seq, for checking anchors, as [`hex_key`]s: 32
    /// bytes each rather than a 64-character string, because a cached walk
    /// keeps them for the life of the store. Only a contiguous prefix is
    /// kept, so a hand-edited huge seq cannot force a huge allocation.
    heads: Vec<Key>,
    genesis_seen: bool,
    /// Lines an evidence pack cannot place in a window: not a JSON object,
    /// or no integer `seq` or `at_ns`.
    unplaceable: u64,
    /// The byte offset of every line a pack can place, by its `at_ns`, so a
    /// pack finds its window's entries by lookup, without parsing the chain
    /// again or passing over the index.
    times: TimeIndex,
    /// The ledger file did not exist when walked.
    missing: bool,
}

/// A head or a subject digest, as the walk and the cross-check keep them:
/// see [`hex_key`].
type Key = [u8; 32];

/// Arrivals findable by time, each `(at_ns, key)` with `key` increasing in
/// arrival order (a byte offset, a position).
///
/// Arrival order and time order almost always agree — `at_ns` is the wall
/// clock at append, and appends are serialized — so an arrival no older than
/// the last goes on the end of `sorted`, which then stays sorted both ways
/// and answers a window with two binary searches. One that is older (the
/// clock stepped back, or a hand-edited line) goes to `late`, where a window
/// is a range query.
#[derive(Clone, Debug, Default)]
struct TimeIndex {
    sorted: Vec<(u64, u64)>,
    late: BTreeSet<(u64, u64)>,
}

impl TimeIndex {
    fn push(&mut self, at_ns: u64, key: u64) {
        match self.sorted.last() {
            Some(&(last, _)) if at_ns < last => {
                self.late.insert((at_ns, key));
            }
            _ => self.sorted.push((at_ns, key)),
        }
    }

    /// The keys of arrivals with `at_ns` in `[since, until]` that `keep`
    /// accepts, in arrival order, at most `cap` of them; and whether more
    /// exist.
    ///
    /// O(log n) plus the window, for the arrivals in time order. The late
    /// ones in the window are each looked at (keeping the `cap + 1` lowest
    /// keys), under the deadline: they are rare, but a chain edited into
    /// disorder could hold many, and finding them is not resumable work.
    fn window(
        &self,
        since: u64,
        until: u64,
        cap: usize,
        keep: impl Fn(u64) -> bool,
        deadline: Option<crate::Deadline>,
    ) -> Result<(Vec<u64>, bool)> {
        let lower = self.sorted.partition_point(|&(at_ns, _)| {
            #[cfg(test)]
            probe::examined_one();
            at_ns < since
        });
        let upper = lower
            + self.sorted[lower..].partition_point(|&(at_ns, _)| {
                #[cfg(test)]
                probe::examined_one();
                at_ns <= until
            });
        let mut late = std::collections::BinaryHeap::new();
        for (looked, &(_, key)) in self.late.range((since, 0)..=(until, u64::MAX)).enumerate() {
            #[cfg(test)]
            probe::examined_one();
            if looked > 0 && looked as u64 % WALK_CHECK_INTERVAL == 0 && out_of_budget(deadline) {
                return Err(Error::DeadlineExceeded(format!(
                    "{looked} entries recorded out of time order fall in the window, more than \
                     the budget could look through; narrow the window or raise \
                     --query-deadline-ms"
                )));
            }
            if keep(key) {
                late.push(key);
                if late.len() > cap + 1 {
                    late.pop();
                }
            }
        }
        let mut late = late.into_sorted_vec().into_iter().peekable();
        let mut sorted = self.sorted[lower..upper]
            .iter()
            .map(|&(_, key)| key)
            .filter(|key| keep(*key))
            .peekable();
        let mut keys = Vec::new();
        while keys.len() <= cap {
            let next = match (sorted.peek(), late.peek()) {
                (Some(&left), Some(&right)) if right < left => late.next(),
                (Some(_), _) => sorted.next(),
                (None, _) => late.next(),
            };
            match next {
                Some(key) => keys.push(key),
                None => break,
            }
        }
        let truncated = keys.len() > cap;
        keys.truncate(cap);
        Ok((keys, truncated))
    }
}

/// How far a budgeted step — a chain walk, taking in settled erasures, the
/// erasure cross-check, a file check — got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walked {
    /// To its target.
    Reached,
    /// The deadline passed first; the step stopped where it can resume.
    OutOfBudget,
}

/// A recorded or claimed head, or a subject digest, as the walk and the
/// cross-check compare them: the 32 bytes a well-formed SHA-256 hex string
/// spells, and for anything else the SHA-256 of its text. Equal keys mean
/// equal strings — a malformed string colliding with a well-formed one
/// would be a SHA-256 preimage — so comparing keys is comparing the
/// strings, at a third of the memory.
fn hex_key(text: &str) -> Key {
    fn decode(text: &str) -> Option<Key> {
        if !is_sha256_hex(text) {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let nibble = |digit: u8| match digit {
                b'0'..=b'9' => digit - b'0',
                _ => digit - b'a' + 10,
            };
            *byte = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        Some(bytes)
    }
    decode(text)
        .or_else(|| decode(&payload::sha256_hex(text.as_bytes())))
        .unwrap_or_default()
}

impl ChainWalk {
    fn new() -> Self {
        Self {
            len: 0,
            unterminated: false,
            line_number: 0,
            entries: 0,
            head: None,
            last_checkpoint: None,
            erasures: Vec::new(),
            found: Vec::new(),
            expected: Some((0, GENESIS_PREV.to_owned())),
            heads: Vec::new(),
            genesis_seen: false,
            unplaceable: 0,
            times: TimeIndex::default(),
            missing: false,
        }
    }

    /// Walks on through byte `to` of the ledger at `path` (its end when
    /// `None`), checking the deadline every [`WALK_CHECK_INTERVAL`] lines,
    /// so every call makes at least that much progress. Returns the target,
    /// and whether it was reached.
    ///
    /// A final line before `to` that lacks its newline is taken in all the
    /// same — within a snapshot it is a complete entry the open could not
    /// terminate — and marked, so the next call steps over the newline the
    /// next append writes before its own line.
    fn advance(
        &mut self,
        path: &Path,
        to: Option<u64>,
        deadline: Option<crate::Deadline>,
    ) -> Result<(u64, Walked)> {
        #[cfg(test)]
        probe::walking();
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.missing = true;
                return Ok((self.len, Walked::Reached));
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let to = match to {
            Some(to) => to,
            None => file.metadata()?.len(),
        };
        if self.unterminated && self.len < to {
            file.seek(SeekFrom::Start(self.len))?;
            let mut byte = [0_u8; 1];
            if file.read(&mut byte)? == 1 && byte[0] == b'\n' {
                self.len += 1;
            }
            self.unterminated = false;
        }
        if self.len >= to {
            return Ok((to, Walked::Reached));
        }
        file.seek(SeekFrom::Start(self.len))?;
        let mut reader = BufReader::new(file.take(to - self.len));
        let mut line = Vec::new();
        let mut walked = 0_u64;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                return Ok((to, Walked::Reached));
            }
            self.feed(&line, self.len);
            self.len += read as u64;
            if !line.ends_with(b"\n") {
                self.unterminated = true;
                return Ok((to, Walked::Reached));
            }
            walked += 1;
            if self.len < to && walked % WALK_CHECK_INTERVAL == 0 && out_of_budget(deadline) {
                return Ok((to, Walked::OutOfBudget));
            }
        }
    }

    /// Checks one line starting at byte `offset`.
    fn feed(&mut self, line: &[u8], offset: u64) {
        #[cfg(test)]
        probe::parsed_one();
        self.line_number += 1;
        let line_number = self.line_number;
        if line.iter().all(u8::is_ascii_whitespace) {
            self.found.push(format!("line {line_number}: blank"));
            return;
        }
        self.entries += 1;
        let entry = match serde_json::from_slice::<Value>(line) {
            Ok(Value::Object(map)) => map,
            _ => {
                self.found.push(format!(
                    "line {line_number}: not a JSON object; the chain cannot be followed past it"
                ));
                self.expected = None;
                self.unplaceable += 1;
                return;
            }
        };
        let Some(seq) = entry.get("seq").and_then(Value::as_u64) else {
            self.found
                .push(format!("line {line_number}: no integer seq"));
            self.expected = None;
            self.unplaceable += 1;
            return;
        };
        match entry.get("at_ns").and_then(Value::as_u64) {
            Some(at_ns) => self.times.push(at_ns, offset),
            None => self.unplaceable += 1,
        }
        let name = format!("seq {seq}");
        let recorded_head = entry.get("head").and_then(Value::as_str).map(str::to_owned);
        let prev = entry.get("prev").and_then(Value::as_str);
        if let Some((want_seq, want_prev)) = &self.expected {
            if seq != *want_seq {
                self.found.push(format!(
                    "{name} (line {line_number}): out of order, seq {want_seq} was expected"
                ));
            } else if prev != Some(want_prev.as_str()) {
                self.found.push(format!(
                    "{name}: prev does not match the head of the entry before it"
                ));
            }
        }
        let recomputed = compute_head(prev.unwrap_or(""), &Value::Object(entry.clone()));
        match &recorded_head {
            None => self.found.push(format!("{name}: no head")),
            Some(head) if *head != recomputed => self.found.push(format!(
                "{name}: head does not match its contents (recorded {head}, recomputed {recomputed})"
            )),
            Some(_) => {}
        }
        check_body(
            &entry,
            seq,
            &self.heads,
            &mut self.genesis_seen,
            &mut self.found,
        );
        match entry.get("kind").and_then(Value::as_str) {
            Some("checkpoint") => {
                if let (Some(generation), Some(manifest)) = (
                    entry.get("generation").and_then(Value::as_u64),
                    entry.get("manifest_sha256").and_then(Value::as_str),
                ) {
                    self.last_checkpoint = Some((seq, generation, manifest.to_owned()));
                }
            }
            Some("erasure") => {
                if let (Some(digest), Some(erasure_id)) = (
                    entry.get("subject_digest").and_then(Value::as_str),
                    entry.get("erasure_id").and_then(Value::as_u64),
                ) {
                    self.erasures.push(LedgeredErasure {
                        seq,
                        erasure_id,
                        subject_digest: hex_key(digest),
                        generation: entry.get("generation").and_then(Value::as_u64),
                        subject_kind: entry
                            .get("subject_kind")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned(),
                        tenant: entry
                            .get("tenant")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
            }
            _ => {}
        }
        let head = recorded_head.unwrap_or(recomputed);
        if seq == self.heads.len() as u64 {
            self.heads.push(hex_key(&head));
        }
        self.expected = Some((seq.saturating_add(1), head.clone()));
        self.head = Some(head);
    }

    /// Every problem the walk found, plus those only a whole walk states.
    #[cfg(test)]
    fn problems(&self) -> Vec<String> {
        self.problems_up_to(usize::MAX).0
    }

    /// The first `cap` problems the walk found, with those only a whole
    /// walk states, and how many there are in all: a damaged chain's
    /// findings are kept with the walk, and a pack copies only the ones it
    /// names.
    fn problems_up_to(&self, cap: usize) -> (Vec<String>, usize) {
        if self.missing {
            return (vec![format!("{LOG_NAME} is missing")], 1);
        }
        let genesis = (!self.genesis_seen).then(|| "no genesis entry at seq 0".to_owned());
        let total = self.found.len() + usize::from(genesis.is_some());
        let problems = self
            .found
            .iter()
            .cloned()
            .chain(genesis)
            .take(cap)
            .collect();
        (problems, total)
    }
}

/// What [`Store::evidence`] takes from the state packs keep: everything that
/// needs [`Ledger::evidence`] held, so the rest of the pack is assembled
/// without it.
struct EvidenceChain {
    /// The snapshot's length and the generation the pack answers for: the
    /// [`Pin`] it lets go of once the pack is whole, when it is that one.
    target: (u64, u64),
    /// The tip at the snapshot the walk ran to.
    head: LedgerHead,
    verification: Verification,
    /// Byte offsets of the window's entries, in file order, at most
    /// [`EVIDENCE_MAX_ENTRIES`].
    window: Vec<u64>,
    /// More entries fall in the window than `window` holds.
    window_truncated: bool,
    /// Lines of the chain a pack cannot place in any window.
    unplaceable: u64,
    /// The erasures the pack lists, oldest first, at most
    /// [`EVIDENCE_MAX_ERASURES`].
    erasures: Vec<ListedErasure>,
    /// More erasures fall in the window than `erasures` holds.
    erasures_truncated: bool,
}

/// One erasure an evidence pack lists.
struct ListedErasure {
    id: u64,
    subject_kind: &'static str,
    published_at_ns: u64,
}

/// What evidence packs keep between requests; see [`Store::evidence`].
#[derive(Debug, Default)]
struct Evidence {
    /// The live tombstone log's settle records taken in so far.
    settles: SettleIndex,
    /// The completed walk packs serve, extending it by what was appended
    /// since; with its cross-check.
    served: Option<CachedWalk>,
    /// A walk from byte 0 under way: the first one, which a pack must
    /// finish before it can answer, or the redo of a served walk past
    /// [`EVIDENCE_VERIFY_REUSE`], which packs advance a slice at a time
    /// while serving the older one. Never discarded for its age.
    refresh: Option<CachedWalk>,
    /// The file check, once a pack has taken a live generation's manifest
    /// in.
    files: Option<FileVerifier>,
    /// The live generation's manifest being taken in: one `files` is not
    /// about yet, which a pack must finish before it can answer, or the one
    /// it is about read again past [`EVIDENCE_VERIFY_REUSE`], which packs
    /// advance a slice at a time while answering from `files`.
    loading: Option<ManifestLoad>,
    /// What packs answer for while a pack that had to take a new
    /// generation's manifest in is unanswered; see [`Pin`].
    pin: Option<Pin>,
    /// File checks replaced or dropped, freed a budget at a time by
    /// [`Self::reclaim`] rather than whole where they were let go.
    retired: Vec<FileVerifier>,
}

/// The snapshot and live generation a pack took when it found it had to
/// take that generation's manifest in, which packs answer for until one
/// does.
///
/// A pack answers for one consistent pair — the chain's prefix and the
/// generation its last checkpoint entry names — and a manifest too long for
/// one budget is taken in across requests. Snapshotting afresh on each
/// retry let a checkpoint landing between any two of them make the load
/// about a generation no longer live, and starting over on the new one met
/// the next checkpoint the same way: however many retries a client made, a
/// store checkpointing as often as that never produced a pack. Kept
/// instead, the load finishes — its manifest is an open handle, immutable
/// — and the pack answers for the pair as it stood, which its head
/// (`ledger.head`) states.
///
/// Let go when a pack answers; when a confirmation finds its generation is
/// no longer live (what it found missing may be what the store swept, so it
/// cannot be answered for: the next pack takes the generation live then);
/// when its manifest does not decode; and once its manifest is in and it is
/// older than [`EVIDENCE_VERIFY_REUSE`], so a pack is never about a chain
/// much older than that window — what remains carries over to the next
/// generation by unchanged manifest entry.
#[derive(Debug)]
struct Pin {
    snapshot: Snapshot,
    generation: u64,
    /// When it was taken, on the ledger's clock.
    at: Instant,
}

impl Evidence {
    /// Holding the maintenance lock: opens the live generation's manifest
    /// when packs need it read — no file check of `live` yet, or one read
    /// longer ago than the reuse window — unless that read is under way. A
    /// read of any other generation is displaced, the store having moved
    /// past it, and returned: for the caller to [`Self::retire_load`] once
    /// the lock is released, since what it decoded is a state per file.
    fn open_manifest(
        &mut self,
        directory: &Path,
        live: u64,
        now: Instant,
    ) -> Result<Option<ManifestLoad>> {
        if let Some(load) = &self.loading {
            if load.generation == live && !load.stale_failure(now) {
                return Ok(None);
            }
        }
        let due = self.files.as_ref().map_or(true, |files| {
            files.generation != live
                || now.saturating_duration_since(files.read_at) > EVIDENCE_VERIFY_REUSE
        });
        let opened = match due {
            true => Some(ManifestLoad::open(directory, live, now)?),
            false => None,
        };
        Ok(std::mem::replace(&mut self.loading, opened))
    }

    /// Advances the manifest under way, and installs its check once it is
    /// taken in; the check it replaces is retired.
    fn load_manifest(&mut self, deadline: Option<crate::Deadline>) -> Result<Walked> {
        let Some(load) = self.loading.as_mut() else {
            return Ok(Walked::Reached);
        };
        if load.advance(self.files.as_ref(), deadline)? == Walked::OutOfBudget {
            return Ok(Walked::OutOfBudget);
        }
        if let Some(load) = self.loading.take() {
            let (files, replaced) = load.finish(self.files.take());
            self.files = files;
            self.retired.extend(replaced);
        }
        Ok(Walked::Reached)
    }

    /// Lets go of a load the store moved past: what it decoded joins the
    /// checks [`Self::reclaim`] frees, and the rest — its bytes, one
    /// allocation, and its handle — goes now.
    fn retire_load(&mut self, load: ManifestLoad) {
        match load.stage {
            LoadStage::Decoding { built, .. } | LoadStage::Done(built) => self.retired.push(built),
            LoadStage::Reading { .. } | LoadStage::Unchanged | LoadStage::Failed(_) => {}
        }
    }

    /// Frees retired file checks, [`RECLAIM_CHUNK`] files' states between
    /// budget checks, until none is left or the budget is spent — one chunk
    /// at least, so retries free them all.
    ///
    /// A check holds a state per manifested file, freed one allocation at a
    /// time: dropped whole where it was let go, a large store's check is
    /// work no budget sees, and where that is under the maintenance lock,
    /// work checkpoints, compaction and expiry wait for.
    fn reclaim(&mut self, deadline: Option<crate::Deadline>) {
        while let Some(check) = self.retired.last_mut() {
            if check.shed(RECLAIM_CHUNK) {
                self.retired.pop();
            }
            if self.retired.is_empty() || out_of_budget(deadline) {
                return;
            }
        }
    }

    /// Lets go of the [`Pin`] once its manifest is in and it is older than
    /// the reuse window; whether it did.
    fn release_stale_pin(&mut self, now: Instant) -> bool {
        let stale = self.pin.as_ref().is_some_and(|pin| {
            self.files
                .as_ref()
                .is_some_and(|files| files.generation == pin.generation)
                && now.saturating_duration_since(pin.at) > EVIDENCE_VERIFY_REUSE
        });
        if stale {
            self.pin = None;
        }
        stale
    }
}

/// A chain walk packs keep, with its cross-check and when its walk from
/// byte 0 began — the moment its oldest finding was made.
#[derive(Debug)]
struct CachedWalk {
    walk: ChainWalk,
    cross: CrossCheck,
    /// On the ledger's clock ([`Ledger::now`]).
    started: Instant,
    /// The same moment, nanoseconds since the Unix epoch, as packs report it.
    started_at_ns: u64,
    /// The prefix a walk under way must reach to be complete: fixed when it
    /// began, so a walk slower than the appends still finishes.
    target: u64,
}

impl CachedWalk {
    fn new(target: u64, started: Instant, started_at_ns: u64) -> Self {
        Self {
            walk: ChainWalk::new(),
            cross: CrossCheck::default(),
            started,
            started_at_ns,
            target,
        }
    }
}

/// One erasure entry, as the cross-check needs it.
#[derive(Clone, Debug)]
struct LedgeredErasure {
    seq: u64,
    erasure_id: u64,
    subject_digest: Key,
    /// `None` when the entry lacks one; the body check reports that.
    generation: Option<u64>,
    subject_kind: String,
    tenant: Option<String>,
}

/// Walks the first `length` bytes of the ledger at `path` (the whole file
/// when `None`) from byte 0, with no budget, the final line included
/// whether or not it is terminated: a one-shot verification of the chain.
fn verify_chain(path: &Path, length: Option<u64>) -> Result<ChainWalk> {
    let mut walk = ChainWalk::new();
    walk.advance(path, length, None)?;
    Ok(walk)
}

/// One settle record as [`SettleIndex`] keeps it.
#[derive(Clone, Debug)]
struct IndexedSettle {
    id: u64,
    digest: Key,
    /// The generation that published the deletion.
    generation: u64,
    settled_unix_ns: u64,
    subject_kind: &'static str,
    /// The subject's tenant, `None` for a store-global payload subject.
    tenant: Option<String>,
    /// A later position holds this erasure's current settle: it was
    /// settled twice (two completions racing), and only the later counts.
    superseded: bool,
}

/// A tombstone log's settle records, each taken in — its subject digested —
/// once, in settle order (see [`erasure::ErasureLog::settlements`]), and
/// findable by settle time. What the erasure cross-check and a pack's
/// erasure listing read, so neither clones or re-digests the log per
/// request.
#[derive(Debug, Default)]
struct SettleIndex {
    entries: Vec<IndexedSettle>,
    /// Each erasure's latest position, so a second settle of one erasure
    /// supersedes the first.
    latest: HashMap<u64, usize>,
    /// Positions by settle time: every entry, and each tenant's.
    all: TimeIndex,
    tenants: HashMap<String, TimeIndex>,
}

impl SettleIndex {
    /// The index of the settled erasures in `statuses`, for a log no live
    /// store has open (nothing settles twice in what a replay reports).
    fn from_statuses(statuses: &[erasure::ErasureStatus]) -> Self {
        let mut index = Self::default();
        for status in statuses {
            if let Some(settle) = &status.settle {
                index.push(&erasure::Settlement {
                    id: status.erase.id,
                    subject: status.erase.subject.clone(),
                    generation: settle.generation,
                    settled_unix_ns: settle.settled_unix_ns,
                });
            }
        }
        index
    }

    fn push(&mut self, settlement: &erasure::Settlement) {
        #[cfg(test)]
        probe::digested_one();
        let position = self.entries.len();
        if let Some(earlier) = self.latest.insert(settlement.id, position) {
            self.entries[earlier].superseded = true;
        }
        let tenant = settlement.subject.tenant().map(str::to_owned);
        self.all.push(settlement.settled_unix_ns, position as u64);
        if let Some(tenant) = &tenant {
            self.tenants
                .entry(tenant.clone())
                .or_default()
                .push(settlement.settled_unix_ns, position as u64);
        }
        self.entries.push(IndexedSettle {
            id: settlement.id,
            digest: hex_key(&subject_digest(&settlement.subject)),
            generation: settlement.generation,
            settled_unix_ns: settlement.settled_unix_ns,
            subject_kind: subject_kind(&settlement.subject),
            tenant,
            superseded: false,
        });
    }

    /// Holds a position whose erasure has no settle now (see
    /// [`erasure::ErasureLog::settlements`]): never current, so neither the
    /// cross-check nor a listing reads it, but it keeps every later
    /// position where the log's settle order has it.
    fn push_gap(&mut self) {
        self.entries.push(IndexedSettle {
            id: 0,
            digest: Key::default(),
            generation: 0,
            settled_unix_ns: 0,
            subject_kind: "?",
            tenant: None,
            superseded: true,
        });
    }

    /// Takes in `log`'s settle records through position `to`, a chunk at a
    /// time — one lock hold each — checking the deadline between chunks.
    fn sync(
        &mut self,
        log: &erasure::ErasureLog,
        to: usize,
        deadline: Option<crate::Deadline>,
    ) -> Result<Walked> {
        let mut first = true;
        while self.entries.len() < to {
            if !first && out_of_budget(deadline) {
                return Ok(Walked::OutOfBudget);
            }
            first = false;
            let from = self.entries.len();
            let settlements = log.settlements(from, (from + SETTLE_CHUNK).min(to))?;
            if settlements.is_empty() {
                // The log holds fewer than `to`: nothing a caller reading its
                // length first can ask for, but never spin on it.
                break;
            }
            for settlement in &settlements {
                match settlement {
                    Some(settlement) => self.push(settlement),
                    None => self.push_gap(),
                }
            }
        }
        Ok(Walked::Reached)
    }

    /// The current settles of `tenant`'s subjects (every subject, for
    /// `None`) settled within `[since, until]`, oldest first, at most `cap`;
    /// and whether more exist.
    fn window(
        &self,
        tenant: Option<&str>,
        since: u64,
        until: u64,
        cap: usize,
        deadline: Option<crate::Deadline>,
    ) -> Result<(Vec<&IndexedSettle>, bool)> {
        let index = match tenant {
            None => &self.all,
            Some(tenant) => match self.tenants.get(tenant) {
                Some(index) => index,
                None => return Ok((Vec::new(), false)),
            },
        };
        let current = |position: u64| !self.entries[position as usize].superseded;
        let (positions, truncated) = index.window(since, until, cap, current, deadline)?;
        Ok((
            positions
                .into_iter()
                .map(|position| &self.entries[position as usize])
                .collect(),
            truncated,
        ))
    }
}

/// A settled publication as the cross-check keys the ones the walked chain
/// lacks: `(erasure_id, generation, subject_digest)`, so they sort by id.
type Unledgered = (u64, u64, Key);

/// What [`Ledger::landing`] reads under the tip lock: the erasure entries
/// appended past a snapshot's prefix, as [`Unledgered`] keys, and the ids of
/// the erasures still being appended.
struct Landing {
    appended: Vec<Unledgered>,
    settling: Vec<u64>,
}

/// The erasure cross-check of one walk against the settled erasures, kept
/// up to date as either grows, so a pack pays for what is new on each side
/// rather than for every erasure ever settled and ledgered.
///
/// It holds, for what it has taken in, exactly what [`assemble`] reports:
/// the erasure entries no settlement vouches for (`undone`) and the settled
/// publications the walked chain lacks (`unledgered`). Both directions are
/// monotone while a store is open — a live tombstone log only gains
/// settles, so a subject's newest settled generation only rises, and a walk
/// only gains entries — which is what lets each new settle or entry update
/// them alone. The one change that is not monotone, an erasure settled a
/// second time with a different generation (two completions of one erasure
/// racing), starts the check over, from memory.
///
/// Both kinds of finding are also kept ordered, in all and by the tenant
/// each concerns, so a pack itemizes the first few its reader may see and
/// counts the rest without collecting, sorting or formatting them all: a
/// restore that undoes many erasures must not make every pack — least of all
/// one for a tenant those erasures never concerned — pay for each of them.
#[derive(Clone, Debug, Default)]
struct CrossCheck {
    /// Erasure entries of the walk taken in: `walk.erasures[..entries]`.
    entries: usize,
    /// Settle-index positions taken in: `settles.entries[..settles]`.
    settles: usize,
    /// The settle taken in for each erasure id: `(digest, generation)`.
    settled: HashMap<u64, (Key, u64)>,
    /// The newest settled generation of each subject.
    newest: HashMap<Key, u64>,
    /// Every publication the walked chain ledgers:
    /// `(erasure_id, subject_digest, generation)`.
    ledgered: HashSet<(u64, Key, u64)>,
    /// Entries no settlement vouches for, by subject: `(generation, index
    /// into walk.erasures)`.
    undone: HashMap<Key, Vec<(u64, usize)>>,
    /// The same entries' indexes into walk.erasures, in chain order.
    undone_order: BTreeSet<usize>,
    /// The same again, by the tenant each concerns (`None`: a store-global
    /// payload subject).
    undone_scoped: HashMap<Option<String>, BTreeSet<usize>>,
    /// Settled publications the walked chain lacks, with their settle-index
    /// positions.
    unledgered: BTreeMap<Unledgered, usize>,
    /// The same publications, by the tenant each concerns.
    unledgered_scoped: HashMap<Option<String>, BTreeSet<Unledgered>>,
}

impl CrossCheck {
    /// Takes in what `walk` and `settles` gained since the last call,
    /// settles first, checking the deadline every [`WALK_CHECK_INTERVAL`]
    /// steps.
    fn sync(
        &mut self,
        walk: &ChainWalk,
        settles: &SettleIndex,
        deadline: Option<crate::Deadline>,
    ) -> Walked {
        let mut steps = 0_u64;
        loop {
            if self.settles < settles.entries.len() {
                let entry = &settles.entries[self.settles];
                if !entry.superseded && !self.settle(self.settles, entry, &walk.erasures) {
                    *self = Self::default();
                    continue;
                }
                self.settles += 1;
            } else if self.entries < walk.erasures.len() {
                self.ledger(self.entries, &walk.erasures[self.entries], settles);
                self.entries += 1;
            } else {
                return Walked::Reached;
            }
            #[cfg(test)]
            probe::examined_one();
            steps += 1;
            if steps % WALK_CHECK_INTERVAL == 0 && out_of_budget(deadline) {
                return Walked::OutOfBudget;
            }
        }
    }

    /// Takes in the settle at `position`. False when it replaces a
    /// different settle of the same erasure already taken in — which only
    /// a check started over can follow.
    fn settle(
        &mut self,
        position: usize,
        entry: &IndexedSettle,
        erasures: &[LedgeredErasure],
    ) -> bool {
        match self.settled.get(&entry.id) {
            Some(&taken) if taken == (entry.digest, entry.generation) => return true,
            Some(_) => return false,
            None => {}
        }
        self.settled
            .insert(entry.id, (entry.digest, entry.generation));
        let newest = self.newest.entry(entry.digest).or_insert(0);
        *newest = (*newest).max(entry.generation);
        let newest = *newest;
        if let Some(waiting) = self.undone.get_mut(&entry.digest) {
            let (order, scoped) = (&mut self.undone_order, &mut self.undone_scoped);
            waiting.retain(|&(generation, index)| {
                let holds = generation > newest;
                if !holds {
                    order.remove(&index);
                    remove_scoped(scoped, &erasures[index].tenant, &index);
                }
                holds
            });
            if waiting.is_empty() {
                self.undone.remove(&entry.digest);
            }
        }
        if !self
            .ledgered
            .contains(&(entry.id, entry.digest, entry.generation))
        {
            let key = (entry.id, entry.generation, entry.digest);
            self.unledgered.insert(key, position);
            self.unledgered_scoped
                .entry(entry.tenant.clone())
                .or_default()
                .insert(key);
        }
        true
    }

    /// Takes in the erasure entry at `index` of the walk.
    fn ledger(&mut self, index: usize, erasure: &LedgeredErasure, settles: &SettleIndex) {
        if let Some(generation) = erasure.generation {
            self.ledgered
                .insert((erasure.erasure_id, erasure.subject_digest, generation));
            let key = (erasure.erasure_id, generation, erasure.subject_digest);
            if let Some(position) = self.unledgered.remove(&key) {
                remove_scoped(
                    &mut self.unledgered_scoped,
                    &settles.entries[position].tenant,
                    &key,
                );
            }
        }
        let published = erasure.generation.unwrap_or(0);
        if self
            .newest
            .get(&erasure.subject_digest)
            .map_or(true, |newest| *newest < published)
        {
            self.undone
                .entry(erasure.subject_digest)
                .or_default()
                .push((published, index));
            self.undone_order.insert(index);
            self.undone_scoped
                .entry(erasure.tenant.clone())
                .or_default()
                .insert(index);
        }
    }

    /// Of [`Ledger::landing`]'s candidates, the publications this check
    /// holds as unledgered: the ones a verification must not call missing.
    fn landing(&self, landing: &Landing) -> HashSet<Unledgered> {
        let mut found: HashSet<Unledgered> = landing
            .appended
            .iter()
            .filter(|key| self.unledgered.contains_key(key))
            .copied()
            .collect();
        for &id in &landing.settling {
            found.extend(
                self.unledgered
                    .range((id, 0, [0; 32])..=(id, u64::MAX, [u8::MAX; 32]))
                    .map(|(key, _)| *key),
            );
        }
        found
    }

    /// The undone entries `reader` may see (every one for `None`), as
    /// indexes into the walk's erasures in chain order, and how many there
    /// are.
    fn undone_for(&self, reader: Option<&str>) -> (Box<dyn Iterator<Item = usize> + '_>, usize) {
        match reader {
            None => (
                Box::new(self.undone_order.iter().copied()),
                self.undone_order.len(),
            ),
            Some(tenant) => match self.undone_scoped.get(&Some(tenant.to_owned())) {
                Some(own) => (Box::new(own.iter().copied()), own.len()),
                None => (Box::new(std::iter::empty()), 0),
            },
        }
    }

    /// The unledgered publications `reader` may see (every one for `None`),
    /// in id order, and how many there are.
    fn unledgered_for(
        &self,
        reader: Option<&str>,
    ) -> (Box<dyn Iterator<Item = Unledgered> + '_>, usize) {
        match reader {
            None => (
                Box::new(self.unledgered.keys().copied()),
                self.unledgered.len(),
            ),
            Some(tenant) => match self.unledgered_scoped.get(&Some(tenant.to_owned())) {
                Some(own) => (Box::new(own.iter().copied()), own.len()),
                None => (Box::new(std::iter::empty()), 0),
            },
        }
    }
}

/// Removes `item` from `scope`'s set, and the set once it is empty.
fn remove_scoped<T: Ord>(
    sets: &mut HashMap<Option<String>, BTreeSet<T>>,
    scope: &Option<String>,
    item: &T,
) {
    if let Some(set) = sets.get_mut(scope) {
        set.remove(item);
        if set.is_empty() {
            sets.remove(scope);
        }
    }
}

/// The kind-specific checks: required fields present with the right types,
/// genesis first and only once, anchors naming a real earlier head.
fn check_body(
    entry: &Map<String, Value>,
    seq: u64,
    heads: &[Key],
    genesis_seen: &mut bool,
    problems: &mut Vec<String>,
) {
    let name = format!("seq {seq}");
    let text = |key: &str| entry.get(key).and_then(Value::as_str);
    let number = |key: &str| entry.get(key).and_then(Value::as_u64);
    if number("at_ns").is_none() {
        problems.push(format!("{name}: no integer at_ns"));
    }
    match text("kind") {
        Some("genesis") => {
            if seq != 0 || *genesis_seen {
                problems.push(format!(
                    "{name}: a genesis entry that is not the first entry"
                ));
            }
            *genesis_seen = true;
            if text("store_id").is_none() {
                problems.push(format!("{name}: genesis without a store_id"));
            }
        }
        Some("checkpoint") => {
            if number("generation").is_none() || !text("manifest_sha256").is_some_and(is_sha256_hex)
            {
                problems.push(format!(
                    "{name}: checkpoint without an integer generation and a manifest_sha256"
                ));
            }
        }
        Some("erasure") => {
            let tenant_ok = matches!(entry.get("tenant"), Some(Value::String(_) | Value::Null));
            if number("erasure_id").is_none()
                || !tenant_ok
                || text("subject_kind").is_none()
                || !text("subject_digest").is_some_and(is_sha256_hex)
                || number("generation").is_none()
                || text("salt").is_none()
            {
                problems.push(format!(
                    "{name}: erasure without erasure_id, tenant, subject_kind, subject_digest, \
                     generation and salt"
                ));
            }
        }
        Some("anchor") => match (number("target_seq"), text("target_head")) {
            (Some(target), Some(target_head)) if target < seq => match heads.get(target as usize) {
                Some(head) if *head == hex_key(target_head) => {}
                Some(_) => problems.push(format!(
                    "{name}: anchors seq {target}, but its target_head is not the head \
                         recorded at seq {target}"
                )),
                None => problems.push(format!(
                    "{name}: anchors seq {target}, which the chain does not hold in order"
                )),
            },
            (Some(target), Some(_)) => problems.push(format!(
                "{name}: anchors seq {target}, which is not an earlier entry"
            )),
            _ => problems.push(format!("{name}: anchor without target_seq and target_head")),
        },
        Some(other) => problems.push(format!("{name}: unknown kind {other:?}")),
        None => problems.push(format!("{name}: no kind")),
    }
}

/// What [`assemble`] checks a walked chain against, beyond the cross-check:
/// everything about the store that is not the chain — live, gathered around
/// the snapshot that fixes the chain's prefix.
///
/// The settled erasures the cross-check took in were read AFTER that
/// snapshot: an erasure entry is appended only once its settle is durable,
/// so every erasure in the prefix has its settle there unless the log lost
/// it.
struct LiveFacts {
    /// The generation `CURRENT` names.
    generation: u64,
    /// SHA-256 of its manifest, read with it: once the lock is released a
    /// checkpoint may publish a successor and sweep this one away.
    manifest_sha256: String,
    /// The file check of that generation.
    files: FileCheck,
    /// Of the settled publications the walked chain lacks, the ones whose
    /// entry is past the prefix or being appended; see [`Ledger::landing`].
    /// Empty offline, where nothing moves.
    landing: HashSet<Unledgered>,
    /// Lines of the tombstone log that did not parse, which the offline
    /// read skips (the open would refuse the log). Zero live.
    tombstones_unreadable: usize,
    /// An append failure not yet caught up, if any.
    failure: Option<Failure>,
    /// When the walk began at byte 0; see
    /// [`Verification::chain_verified_at_ns`].
    chain_verified_at_ns: u64,
}

/// The problems [`assemble`] names for one reader: each kind up to `cap`,
/// the rest of it counted in one line.
struct Named {
    cap: usize,
    problems: Vec<String>,
    truncated: bool,
}

impl Named {
    /// Names the first `cap` of `problems`, a kind with `total` in all, and
    /// counts the others in the line `rest(unnamed, total)` builds. Only the
    /// named are formatted: `problems` is lazy, so a kind's size costs
    /// nothing here.
    fn kind(
        &mut self,
        problems: impl Iterator<Item = String>,
        total: usize,
        rest: impl FnOnce(usize, usize) -> String,
    ) {
        let before = self.problems.len();
        self.problems.extend(problems.take(self.cap));
        let unnamed = total.saturating_sub(self.problems.len() - before);
        if unnamed > 0 {
            self.problems.push(rest(unnamed, total));
            self.truncated = true;
        }
    }
}

/// Checks a walked chain, and its cross-check against `settles`, against
/// the live facts, naming what it finds as `detail` says. No I/O: the walk,
/// the cross-check and the facts carry everything, which is what lets the
/// walk run outside the maintenance lock the facts were gathered under.
///
/// `intact` is the store's verdict whatever `detail` names, and the work is
/// what is named plus a few counts — never a pass over every problem.
fn assemble(
    chain: &ChainWalk,
    cross: &CrossCheck,
    settles: &SettleIndex,
    live: &LiveFacts,
    detail: Detail<'_>,
) -> Verification {
    let generation = live.generation;
    let manifest_sha256 = live.manifest_sha256.clone();
    let (cap, reader) = match detail {
        Detail::All => (usize::MAX, None),
        Detail::Pack(reader) => (EVIDENCE_MAX_PROBLEMS, reader),
    };
    let mut named = Named {
        cap,
        problems: Vec::new(),
        truncated: false,
    };
    let (chain_problems, chain_total) = chain.problems_up_to(cap);
    named.kind(chain_problems.into_iter(), chain_total, |unnamed, total| {
        format!(
            "{unnamed} more problem(s) with the chain itself are not named here ({total} in all)"
        )
    });
    let matches_ledger = match &chain.last_checkpoint {
        Some((_, ledgered, digest)) if *ledgered == generation && *digest == manifest_sha256 => {
            true
        }
        // Newer than the chain: the window between `CURRENT` moving and the
        // checkpoint entry landing — a crash or a failed append in it, or a
        // restore's install not yet opened. Benign when that is the cause,
        // and the problem says how it heals; still not intact, because
        // nothing yet commits the chain to this generation.
        Some((seq, ledgered, _)) if generation > *ledgered => {
            named.problems.push(format!(
                "seq {seq}: the latest checkpoint entry names generation {ledgered}, but the \
                 live generation {generation} (manifest {manifest_sha256}) is newer and not \
                 ledgered yet. A crash or a failed ledger append between publishing a \
                 generation and ledgering it leaves exactly this, and opening the store (or \
                 its next checkpoint) ledgers the live generation; any other cause is a \
                 discrepancy to investigate"
            ));
            false
        }
        Some((seq, ledgered, digest)) => {
            named.problems.push(format!(
                "seq {seq}: the latest checkpoint entry names generation {ledgered} (manifest \
                 {digest}), but the live generation is {generation} (manifest \
                 {manifest_sha256})"
            ));
            false
        }
        None => {
            named.problems.push(format!(
                "no checkpoint entry; the live generation {generation} is not ledgered"
            ));
            false
        }
    };

    // Every ledgered erasure must still hold. The tombstone log never
    // rotates, so the one ordinary way for a settled erasure to leave it is
    // a restore of a pin taken before it — which also brings the subject's
    // data back, and is exactly what an auditor must not miss.
    //
    // Publication by publication, not subject by subject: erase a subject,
    // let new data for it arrive, pin, erase it again, restore the pin —
    // the first erasure is still settled, the second is gone, and so is
    // what it removed. An entry holds only while the live log has a
    // settled erasure of its subject published by the SAME generation or a
    // later one. Generations are never reused within a data directory (an
    // install takes the next id past both stores), so a later one is a
    // re-application that deleted the subject again, rewound data
    // included; an earlier one deleted only what existed before this
    // entry's own deletion did. The cross-check keeps exactly the entries
    // that fail this (see `CrossCheck`), in chain order, so the report reads
    // that way.
    let (undone, undone_seen) = cross.undone_for(reader);
    let undone = undone.map(|index| {
        let erasure = &chain.erasures[index];
        let (described, since) = match erasure.generation {
            Some(generation) => (
                format!(", published in generation {generation}"),
                format!(" published in generation {generation} or later"),
            ),
            None => (String::new(), String::new()),
        };
        format!(
            "seq {}: erasure {} ({} subject{described}) is ledgered, but the live tombstone log \
             holds no settled erasure of that subject{since}. Restoring a pin taken before an \
             erasure undoes it and brings the subject's data back; re-apply the erasure",
            erasure.seq, erasure.erasure_id, erasure.subject_kind
        )
    });
    named.kind(undone, undone_seen, |unnamed, total| {
        format!(
            "{unnamed} more ledgered erasure(s) that the live tombstone log no longer holds are \
             not named here ({total} in all)"
        )
    });

    // And the other way: every settled erasure must be in the chain. The
    // crash window between a settle record and its entry leaves exactly
    // this gap — the checkpoint entry before it already matches `CURRENT`,
    // so nothing else notices — as does a failed append, until the next
    // open or (after a failed append) checkpoint ledgers it. An entry past
    // the prefix, or on its way, is not missing. In id order.
    let settle_of = |key: &Unledgered| {
        let position = cross.unledgered.get(key)?;
        settles.entries.get(*position)
    };
    let visible = |key: &Unledgered| {
        reader.map_or(true, |tenant| {
            settle_of(key).is_some_and(|settle| settle.tenant.as_deref() == Some(tenant))
        })
    };
    let landing_seen = live.landing.iter().filter(|key| visible(key)).count();
    let (unledgered, unledgered_seen) = cross.unledgered_for(reader);
    let missing = unledgered
        .filter(|key| !live.landing.contains(key))
        .map(|key| {
            let (id, generation, _) = key;
            format!(
                "erasure {id} ({} subject, settled in generation {generation}) is not in the \
                 ledger. A crash or a failed ledger append between settling an erasure and \
                 ledgering it leaves exactly this, and opening the store (or, after a failed \
                 append, its next checkpoint) ledgers it; any other cause is a discrepancy to \
                 investigate",
                settle_of(&key).map_or("?", |settle| settle.subject_kind)
            )
        });
    let missing_seen = unledgered_seen.saturating_sub(landing_seen);
    named.kind(missing, missing_seen, |unnamed, total| {
        format!(
            "{unnamed} more settled erasure(s) that are not in the ledger are not named here \
             ({total} in all)"
        )
    });
    if live.tombstones_unreadable > 0 {
        named.problems.push(format!(
            "{}: {} line(s) do not parse, so the erasures they record were not checked \
             against the ledger (the store refuses to open over them)",
            erasure::LOG_NAME,
            live.tombstones_unreadable
        ));
    }

    if let Some(failure) = &live.failure {
        named.problems.push(format!(
            "{} ledger append(s) failed since {} (most recently, {}). What they would have \
             recorded is ledgered at the next checkpoint or open",
            failure.count, failure.first_at_ns, failure.last_error
        ));
    }

    // Files are store-global names (segments, content-addressed payload
    // paths): a tenant learns how many fail, never which.
    let failing = live.files.failing;
    match reader {
        None => named.kind(
            live.files
                .problems
                .iter()
                .map(|problem| format!("generation {generation}: {problem}")),
            failing,
            |unnamed, total| {
                format!(
                    "generation {generation}: {unnamed} more file(s) that fail verification are \
                     not named here ({total} in all)"
                )
            },
        ),
        Some(_) if failing > 0 => named.problems.push(format!(
            "generation {generation}: {failing} file(s) fail verification"
        )),
        Some(_) => {}
    }
    let missing_total = cross.unledgered.len().saturating_sub(live.landing.len());
    if reader.is_some() {
        let withheld = cross.undone_order.len().saturating_sub(undone_seen)
            + missing_total.saturating_sub(missing_seen);
        if withheld > 0 {
            named.problems.push(format!(
                "{withheld} problem(s) about data outside this tenant are withheld from this pack"
            ));
        }
    }
    let intact = chain_total == 0
        && matches_ledger
        && cross.undone_order.is_empty()
        && missing_total == 0
        && live.tombstones_unreadable == 0
        && live.failure.is_none()
        && failing == 0;
    Verification {
        intact,
        entries: chain.entries,
        head: chain.head.clone(),
        chain_verified_at_ns: live.chain_verified_at_ns,
        problems: named.problems,
        problems_truncated: named.truncated,
        generation: GenerationCheck {
            id: generation,
            manifest_sha256,
            matches_ledger,
            files_verified: failing == 0,
            verified_at_ns: live.files.at_ns,
        },
    }
}

/// Re-digests every file generation `live`'s manifest names, in one piece:
/// the operator's check and the offline one.
fn check_files(directory: &Path, live: u64, at_ns: u64) -> Result<FileCheck> {
    let manifest = generation::load_manifest(&generation::manifest_path(directory, live))?;
    let problems = generation::verify_against(directory, &manifest)?;
    Ok(FileCheck {
        at_ns,
        failing: problems.len(),
        problems,
    })
}

/// What tells one file from another at the same path: device and inode on
/// Unix. Every file the store replaces arrives by a rename over its path,
/// which is a new inode, and an inode held open is never reused — so the
/// same id at a path is the same file, appended to perhaps, never replaced.
#[cfg(unix)]
type FileId = (u64, u64);

/// Elsewhere, the creation time: a file renamed over the path was created
/// after the one it replaced.
#[cfg(not(unix))]
type FileId = u128;

#[cfg(unix)]
fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    let created = metadata.created().ok()?;
    created
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_nanos())
}

/// What a file check found at a file's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Nothing.
    Missing,
    /// A file, named by [`file_id`] where the platform allows.
    File(Option<FileId>),
}

impl Seen {
    /// What is at `path` now.
    fn at(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(Self::File(file_id(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// Whether what was read is what is there `now`: nothing both times, or
    /// one file. Where the platform names no file, a file still there is
    /// taken for the one read, so a problem found stands — as every problem
    /// did before files were told apart.
    fn same(self, now: Self) -> bool {
        match (self, now) {
            (Self::Missing, Self::Missing) => true,
            (Self::File(Some(read)), Self::File(Some(now))) => read == now,
            (Self::File(_), Self::File(_)) => true,
            _ => false,
        }
    }
}

/// A pack's file check: one generation's manifest, and for each file it
/// names, when it was last re-digested and what that found.
///
/// Kept between packs and advanced a budget at a time, OFF the maintenance
/// lock; taking a new generation's manifest in is a [`ManifestLoad`], off
/// the lock and budgeted too. Each file is opened when its turn comes and
/// digested from that handle, so a file part-way through when a budget runs
/// out is continued over the same inode next time — whatever compaction or
/// expiry has renamed over its path since — and what is digested is always
/// one consistent file.
///
/// **A problem found off the lock is provisional.** Between the snapshot and
/// a file's turn the store moves: expiry unlinks a segment or renames a
/// rewritten one over it, and a checkpoint publishes a generation that no
/// longer names the old file. Read after that, the file is missing or
/// changed though nothing is damaged. So before a pack reports a problem,
/// [`Self::confirm`] looks again holding the lock. If the live generation is
/// still this one and the path still holds what was read (or still
/// nothing), a check under the lock would find the same, and the problem
/// stands — as `GET /v1/ledger/verify` reports a file compaction removed
/// before the next checkpoint. If the path holds another file now, that file
/// is digested. If another generation is live, the pack takes that one in
/// and checks what it changed: the store moved past the files it was
/// reading. A file that verifies needs no second look — its bytes digested
/// to the manifested digest.
///
/// A file is digested again once its last digest is older than
/// [`EVIDENCE_VERIFY_REUSE`], oldest first, in the slices packs spend on
/// refreshing; and a new generation carries over every file whose manifest
/// entry is unchanged — path, length, digest and modification time — so a
/// checkpoint costs the files it added, not the store. An append-only log
/// keeps more across a checkpoint: see [`Prefix`].
#[derive(Debug)]
struct FileVerifier {
    generation: u64,
    manifest_sha256: String,
    /// When the manifest was opened, on the ledger's clock: read again once
    /// older than the reuse window, or when the live generation moves.
    read_at: Instant,
    files: Vec<FileState>,
    /// The manifest named its files in strictly increasing path order, as
    /// every manifest this store writes does: a later check finds a file's
    /// state here by binary search. One edited out of order carries nothing
    /// over, and its files are digested afresh.
    sorted: bool,
    /// Files never digested against their current manifest entry.
    unchecked: BTreeSet<usize>,
    /// Files digested, by when that digest began: the first is the one due
    /// next, and the oldest finding the check rests on.
    by_age: BTreeSet<(Instant, usize)>,
    /// Files whose last digest found a problem.
    failing: BTreeSet<usize>,
    /// Of `failing`, the files whose problem [`Self::confirm`] has not yet
    /// held up under the maintenance lock.
    provisional: BTreeSet<usize>,
    /// The file being digested, held open across packs.
    hashing: Option<Hashing>,
    /// Append-only logs digested through a prefix, each held open with its
    /// digest's state, by file index.
    prefixes: BTreeMap<usize, Prefix>,
}

#[derive(Debug)]
struct FileState {
    entry: generation::ManifestFile,
    checked: Option<FileChecked>,
}

#[cfg(test)]
impl Drop for FileState {
    fn drop(&mut self) {
        probe::freed_file();
    }
}

/// One file's last digest: when it began, what it found, and the file it
/// read.
#[derive(Clone, Debug)]
struct FileChecked {
    at: Instant,
    at_ns: u64,
    problem: Option<String>,
    seen: Seen,
}

/// A digest under way.
struct Hashing {
    index: usize,
    file: File,
    id: Option<FileId>,
    hasher: payload::Sha256,
    /// Bytes digested so far.
    hashed: u64,
    /// Bytes still to digest of an append-only log's manifested prefix;
    /// `None` for a file read to its end.
    remaining: Option<u64>,
    /// It continues a [`Prefix`] digested against an earlier generation's
    /// entry, so a mismatch may only mean that prefix changed in place since
    /// it was read: the file is digested again from byte 0 before anything
    /// is said about it.
    continued: bool,
    at: Instant,
    at_ns: u64,
}

impl Hashing {
    fn into_prefix(self) -> Prefix {
        Prefix {
            file: self.file,
            id: self.id,
            hasher: self.hasher,
            hashed: self.hashed,
            at: self.at,
            at_ns: self.at_ns,
        }
    }
}

impl std::fmt::Debug for Hashing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hashing")
            .field("index", &self.index)
            .field("hashed", &self.hashed)
            .field("remaining", &self.remaining)
            .field("continued", &self.continued)
            .finish_non_exhaustive()
    }
}

/// An append-only log digested through its first `hashed` bytes: the open
/// file, and the digest's state there.
///
/// Every checkpoint after an append names a longer prefix of the same log,
/// with a new digest, so its entry is never unchanged. A log too long for
/// one budget, appended to and checkpointed before every retry, would be
/// digested from byte 0 each time and never finish, though the retries
/// together read more than enough. Kept instead, by file index across
/// generations, the state continues over the longer prefix. That is sound
/// while the bytes already digested are the ones still there: the path must
/// still name this file — the open handle is the identity, and a log the
/// store rewrote (expiry, an erasure) is renamed over the path as a new
/// file — and its manifested prefix must be no shorter. An edit in place is
/// what neither shows, so a continued digest that disagrees with the
/// manifest is redone from byte 0 before it is reported, and a continued
/// result is as old as its first byte read, so the reuse window redoes it
/// from byte 0 in time like any other.
struct Prefix {
    file: File,
    id: Option<FileId>,
    hasher: payload::Sha256,
    hashed: u64,
    /// When the digest of the prefix began.
    at: Instant,
    at_ns: u64,
}

impl std::fmt::Debug for Prefix {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Prefix")
            .field("hashed", &self.hashed)
            .finish_non_exhaustive()
    }
}

/// How starting a file's digest went.
enum Begun {
    /// Settled without digesting: missing, or the wrong length.
    Found(Option<String>, Instant, u64, Seen),
    Hashing(Hashing),
}

/// What [`FileVerifier::confirm`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirmed {
    /// Every provisional problem holds.
    Held,
    /// Some path holds another file now (or a file now, or none): those
    /// files are to be digested again.
    Changed,
    /// Another generation is live.
    Moved,
}

/// Whether two manifest entries describe one file unchanged: path, length,
/// digest and modification time.
fn unchanged(old: &generation::ManifestFile, new: &generation::ManifestFile) -> bool {
    old.path == new.path
        && old.bytes == new.bytes
        && old.sha256 == new.sha256
        && old.modified_unix_ns == new.modified_unix_ns
}

impl FileVerifier {
    /// An empty check of generation `generation`, whose manifest digests to
    /// `manifest_sha256` and was opened at `read_at`, for a
    /// [`ManifestLoad`] to fill.
    fn empty(generation: u64, manifest_sha256: String, read_at: Instant) -> Self {
        Self {
            generation,
            manifest_sha256,
            read_at,
            files: Vec::new(),
            sorted: true,
            unchecked: BTreeSet::new(),
            by_age: BTreeSet::new(),
            failing: BTreeSet::new(),
            provisional: BTreeSet::new(),
            hashing: None,
            prefixes: BTreeMap::new(),
        }
    }

    /// Takes in one manifest entry, carrying over what `previous` found
    /// about the same file under an unchanged entry, and when.
    fn push(&mut self, entry: generation::ManifestFile, previous: Option<&FileVerifier>) {
        #[cfg(test)]
        probe::manifest_entry();
        let index = self.files.len();
        if self
            .files
            .last()
            .is_some_and(|last| last.entry.path >= entry.path)
        {
            self.sorted = false;
        }
        let carried = previous.and_then(|previous| {
            let old = previous.find(&entry.path)?;
            let state = &previous.files[old];
            if !unchanged(&state.entry, &entry) {
                return None;
            }
            Some((state.checked.clone()?, previous.provisional.contains(&old)))
        });
        let checked = match carried {
            Some((checked, provisional)) => {
                self.by_age.insert((checked.at, index));
                if checked.problem.is_some() {
                    self.failing.insert(index);
                    if provisional {
                        self.provisional.insert(index);
                    }
                }
                Some(checked)
            }
            None => {
                self.unchecked.insert(index);
                None
            }
        };
        self.files.push(FileState { entry, checked });
    }

    /// The index of the file at `path`, by binary search; `None` when this
    /// manifest was not in path order.
    fn find(&self, path: &str) -> Option<usize> {
        if !self.sorted {
            return None;
        }
        self.files
            .binary_search_by(|state| state.entry.path.as_str().cmp(path))
            .ok()
    }

    /// Takes over the files `previous` — the check this one replaces —
    /// holds open: its digest under way, when that file's entry is
    /// unchanged, and each append-only log's digested prefix, which a
    /// longer manifested prefix of the same log continues (see [`Prefix`]).
    /// A handful of files, each found by path. The rest of `previous`, a
    /// state per file, is left for the caller to retire.
    fn adopt(&mut self, previous: &mut FileVerifier) {
        let files = &previous.files;
        let mut prefixes: Vec<(usize, Prefix)> =
            std::mem::take(&mut previous.prefixes).into_iter().collect();
        if let Some(hashing) = previous.hashing.take() {
            let old = &files[hashing.index].entry;
            match self.find(&old.path) {
                Some(index) if unchanged(old, &self.files[index].entry) => {
                    self.hashing = Some(Hashing { index, ..hashing });
                }
                // An append-only log's digest under way is a prefix too.
                Some(_) if hashing.remaining.is_some() => {
                    prefixes.push((hashing.index, hashing.into_prefix()));
                }
                _ => {}
            }
        }
        for (old, prefix) in prefixes {
            let path = &files[old].entry.path;
            if let Some(index) = self.find(path) {
                if generation::is_append_only(path)
                    && self.files[index].entry.bytes >= prefix.hashed
                {
                    self.prefixes.insert(index, prefix);
                }
            }
        }
    }

    /// Digests files until none is unchecked, or the budget is spent.
    fn check_unchecked(
        &mut self,
        directory: &Path,
        deadline: Option<crate::Deadline>,
        ledger: &Ledger,
    ) -> Result<Walked> {
        let mut spent = 0_u64;
        loop {
            if self.unchecked.is_empty() {
                return Ok(Walked::Reached);
            }
            let index = match &self.hashing {
                Some(hashing) => hashing.index,
                None => match self.unchecked.first() {
                    Some(&index) => index,
                    None => return Ok(Walked::Reached),
                },
            };
            if self.check(index, directory, deadline, ledger, &mut spent, false)?
                == Walked::OutOfBudget
            {
                // The budget may have run out just as the last file was
                // recorded.
                return Ok(match self.unchecked.is_empty() {
                    true => Walked::Reached,
                    false => Walked::OutOfBudget,
                });
            }
        }
    }

    /// Digests again from byte 0, oldest first, the files whose last digest
    /// is older than the reuse window, until none is due or the budget is
    /// spent.
    fn recheck_due(
        &mut self,
        directory: &Path,
        deadline: Option<crate::Deadline>,
        ledger: &Ledger,
    ) -> Result<()> {
        let now = ledger.now();
        let mut spent = 0_u64;
        loop {
            let index = match &self.hashing {
                Some(hashing) => hashing.index,
                None => match self.by_age.first() {
                    Some(&(at, index))
                        if now.saturating_duration_since(at) > EVIDENCE_VERIFY_REUSE =>
                    {
                        index
                    }
                    _ => return Ok(()),
                },
            };
            if self.check(index, directory, deadline, ledger, &mut spent, true)?
                == Walked::OutOfBudget
            {
                return Ok(());
            }
        }
    }

    /// Digests file `index` — continuing the digest under way when it is
    /// that file's — to its end or until the budget is spent, and records
    /// what it found. `spent` counts bytes since the last budget check,
    /// across files. `fresh` starts from byte 0 even where a [`Prefix`] of
    /// the file is kept.
    fn check(
        &mut self,
        index: usize,
        directory: &Path,
        deadline: Option<crate::Deadline>,
        ledger: &Ledger,
        spent: &mut u64,
        fresh: bool,
    ) -> Result<Walked> {
        let mut hashing = match self.hashing.take() {
            Some(hashing) if hashing.index == index => hashing,
            _ => {
                *spent += FILE_OPEN_COST;
                match self.begin(index, directory, ledger, fresh)? {
                    Begun::Found(problem, at, at_ns, seen) => {
                        self.record(index, at, at_ns, problem, seen);
                        return Ok(self.budget_check(spent, deadline));
                    }
                    Begun::Hashing(hashing) => hashing,
                }
            }
        };
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            let want = match hashing.remaining {
                Some(0) => break,
                Some(remaining) => chunk.len().min(remaining as usize),
                None => chunk.len(),
            };
            let read = hashing.file.read(&mut chunk[..want])?;
            if read == 0 {
                if hashing.remaining.is_some() {
                    if hashing.continued {
                        self.forget(index);
                        return Ok(self.budget_check(spent, deadline));
                    }
                    let path = &self.files[index].entry.path;
                    let problem = format!("{path}: ended before its manifested length");
                    let seen = Seen::File(hashing.id);
                    self.record(index, hashing.at, hashing.at_ns, Some(problem), seen);
                    return Ok(self.budget_check(spent, deadline));
                }
                break;
            }
            hashing.hasher.update(&chunk[..read]);
            hashing.hashed += read as u64;
            #[cfg(test)]
            probe::hashed(read as u64);
            if let Some(remaining) = &mut hashing.remaining {
                *remaining -= read as u64;
            }
            *spent += read as u64;
            if self.budget_check(spent, deadline) == Walked::OutOfBudget {
                self.hashing = Some(hashing);
                return Ok(Walked::OutOfBudget);
            }
        }
        let entry = &self.files[index].entry;
        let problem = (hashing.hasher.clone().finalize_hex() != entry.sha256)
            .then(|| format!("{}: digest mismatch", entry.path));
        if problem.is_some() && hashing.continued {
            self.forget(index);
            return Ok(Walked::Reached);
        }
        let (at, at_ns, seen) = (hashing.at, hashing.at_ns, Seen::File(hashing.id));
        if hashing.remaining.is_some() {
            self.prefixes.insert(index, hashing.into_prefix());
        }
        self.record(index, at, at_ns, problem, seen);
        Ok(Walked::Reached)
    }

    /// Checks the budget once `spent` reaches [`FILE_CHECK_BYTES`].
    fn budget_check(&self, spent: &mut u64, deadline: Option<crate::Deadline>) -> Walked {
        if *spent < FILE_CHECK_BYTES {
            return Walked::Reached;
        }
        *spent = 0;
        match out_of_budget(deadline) {
            true => Walked::OutOfBudget,
            false => Walked::Reached,
        }
    }

    /// Opens file `index` and checks its length, as
    /// [`generation::verify_against`] does: an append-only log may have
    /// grown past its manifested length, and is digested over that prefix;
    /// anything else must be exactly its manifested length. Unless `fresh`,
    /// an append-only log's kept [`Prefix`] is continued where that is
    /// sound.
    fn begin(
        &mut self,
        index: usize,
        directory: &Path,
        ledger: &Ledger,
        fresh: bool,
    ) -> Result<Begun> {
        #[cfg(test)]
        probe::hashing();
        let path = directory.join(
            self.files[index]
                .entry
                .path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        if let Some(prefix) = self.prefixes.remove(&index).filter(|_| !fresh) {
            if let Some(hashing) = self.continue_prefix(index, prefix, &path)? {
                return Ok(Begun::Hashing(hashing));
            }
        }
        let entry = &self.files[index].entry;
        let (at, at_ns) = (ledger.now(), ledger.unix_now_ns());
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Begun::Found(
                    Some(format!("{}: missing", entry.path)),
                    at,
                    at_ns,
                    Seen::Missing,
                ))
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let metadata = file.metadata()?;
        let (length, id) = (metadata.len(), file_id(&metadata));
        let append_only = generation::is_append_only(&entry.path);
        if length != entry.bytes && !(append_only && length > entry.bytes) {
            return Ok(Begun::Found(
                Some(format!(
                    "{}: {length} bytes on disk, {} in the manifest",
                    entry.path, entry.bytes
                )),
                at,
                at_ns,
                Seen::File(id),
            ));
        }
        Ok(Begun::Hashing(Hashing {
            index,
            file,
            id,
            hasher: payload::Sha256::new(),
            hashed: 0,
            remaining: append_only.then_some(entry.bytes),
            continued: false,
            at,
            at_ns,
        }))
    }

    /// The digest of file `index` continuing `prefix`, when that is sound
    /// (see [`Prefix`]): the path still names the file the prefix was read
    /// from, and that file holds the manifest's prefix, which is no shorter
    /// than what was digested. `None` otherwise: the file is digested from
    /// byte 0.
    fn continue_prefix(
        &self,
        index: usize,
        mut prefix: Prefix,
        path: &Path,
    ) -> Result<Option<Hashing>> {
        let entry = &self.files[index].entry;
        if prefix.id.is_none()
            || prefix.hashed > entry.bytes
            || Seen::at(path)? != Seen::File(prefix.id)
            || prefix.file.metadata()?.len() < entry.bytes
        {
            return Ok(None);
        }
        prefix.file.seek(SeekFrom::Start(prefix.hashed))?;
        Ok(Some(Hashing {
            index,
            file: prefix.file,
            id: prefix.id,
            hasher: prefix.hasher,
            hashed: prefix.hashed,
            remaining: Some(entry.bytes - prefix.hashed),
            continued: true,
            at: prefix.at,
            at_ns: prefix.at_ns,
        }))
    }

    fn record(
        &mut self,
        index: usize,
        at: Instant,
        at_ns: u64,
        problem: Option<String>,
        seen: Seen,
    ) {
        if let Some(previous) = &self.files[index].checked {
            self.by_age.remove(&(previous.at, index));
        }
        self.unchecked.remove(&index);
        match problem.is_some() {
            true => {
                self.failing.insert(index);
                self.provisional.insert(index);
            }
            false => {
                self.failing.remove(&index);
                self.provisional.remove(&index);
            }
        }
        self.by_age.insert((at, index));
        self.files[index].checked = Some(FileChecked {
            at,
            at_ns,
            problem,
            seen,
        });
    }

    /// Sends file `index` back to be digested from byte 0: what was found
    /// about it no longer stands, and no digest of it kept or under way is
    /// continued.
    fn forget(&mut self, index: usize) {
        if let Some(previous) = self.files[index].checked.take() {
            self.by_age.remove(&(previous.at, index));
        }
        self.failing.remove(&index);
        self.provisional.remove(&index);
        self.prefixes.remove(&index);
        if self
            .hashing
            .as_ref()
            .is_some_and(|hashing| hashing.index == index)
        {
            self.hashing = None;
        }
        self.unchecked.insert(index);
    }

    /// Whether a problem awaits [`Self::confirm`].
    fn unconfirmed(&self) -> bool {
        !self.provisional.is_empty()
    }

    /// Settles up to `batch` provisional problems, in path order, against
    /// what is there now; the rest wait for the next call. Called holding
    /// the maintenance lock, so nothing replaces a file meanwhile; `live` is
    /// the generation `CURRENT` names. One metadata read per problem, each
    /// found by a digest a budget paid for — but problems found over many
    /// requests all wait for this, so a hold settles a batch of them, never
    /// however many accumulated.
    fn confirm(&mut self, directory: &Path, live: u64, batch: usize) -> Result<Confirmed> {
        if live != self.generation {
            return Ok(Confirmed::Moved);
        }
        let mut changed = false;
        // Each one settled leaves the set, held or forgotten, so the next
        // is always the first.
        for _ in 0..batch {
            let Some(&index) = self.provisional.first() else {
                break;
            };
            #[cfg(test)]
            probe::confirmed_one();
            let path = directory.join(
                self.files[index]
                    .entry
                    .path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            );
            let now = Seen::at(&path)?;
            let holds = self.files[index]
                .checked
                .as_ref()
                .is_some_and(|checked| checked.seen.same(now));
            match holds {
                true => {
                    self.provisional.remove(&index);
                }
                false => {
                    self.forget(index);
                    changed = true;
                }
            }
        }
        Ok(match changed {
            true => Confirmed::Changed,
            false => Confirmed::Held,
        })
    }

    /// Files digested against their current entry, of all the manifest
    /// names.
    fn progress(&self) -> (usize, usize) {
        (self.files.len() - self.unchecked.len(), self.files.len())
    }

    /// The check as a verification reports it, naming at most
    /// [`EVIDENCE_MAX_PROBLEMS`] failing files. Only meaningful once no file
    /// is unchecked and no problem unconfirmed.
    fn result(&self, now_ns: u64) -> FileCheck {
        let problems = self
            .failing
            .iter()
            .filter_map(|&index| self.files[index].checked.as_ref()?.problem.clone())
            .take(EVIDENCE_MAX_PROBLEMS)
            .collect();
        let at_ns = self
            .by_age
            .first()
            .and_then(|&(_, index)| self.files[index].checked.as_ref())
            .map_or(now_ns, |checked| checked.at_ns);
        FileCheck {
            at_ns,
            problems,
            failing: self.failing.len(),
        }
    }

    /// Frees the states of up to `count` of this check's files, last first,
    /// and as many entries of each of its indexes; true once nothing of it
    /// is left but its shell. For a check being disposed of
    /// ([`Evidence::reclaim`]): what is left is no longer a check.
    fn shed(&mut self, count: usize) -> bool {
        self.files.truncate(self.files.len().saturating_sub(count));
        for _ in 0..count {
            let popped = self.unchecked.pop_last().is_some()
                | self.by_age.pop_last().is_some()
                | self.failing.pop_last().is_some()
                | self.provisional.pop_last().is_some();
            if !popped {
                break;
            }
        }
        // Open files, a handful: see `adopt`.
        self.hashing = None;
        self.prefixes.clear();
        self.files.is_empty()
            && self.unchecked.is_empty()
            && self.by_age.is_empty()
            && self.failing.is_empty()
            && self.provisional.is_empty()
    }
}

/// A generation's manifest being taken in, a budget at a time and off the
/// maintenance lock, toward the [`FileVerifier`] packs check that
/// generation with.
///
/// Only opening the manifest takes the lock: a checkpoint's sweep could
/// otherwise remove it first. A manifest is never written again once renamed
/// into place, and an open handle reads its bytes even after the sweep
/// unlinks it, so the handle is an immutable snapshot of exactly that
/// generation's manifest, for one open under the lock however large it is.
/// Reading and digesting the bytes, decoding an entry per file and carrying
/// over what was known about each are O(files): each stops at the budget,
/// resumes where it stopped on the next pack, and takes each entry in once.
/// A load packs must finish before they can answer is kept to its
/// generation however far the store moves meanwhile ([`Pin`]), so it always
/// finishes.
struct ManifestLoad {
    generation: u64,
    /// When the manifest was opened, on the ledger's clock.
    opened: Instant,
    /// The manifest's bytes read so far.
    bytes: Vec<u8>,
    stage: LoadStage,
}

enum LoadStage {
    /// Reading from the handle opened under the lock, digesting as it goes.
    Reading { file: File, hasher: payload::Sha256 },
    /// Decoding the bytes, an entry at a time.
    Decoding {
        built: FileVerifier,
        parse: ManifestParse,
    },
    /// The bytes are those of the manifest the check already holds.
    Unchanged,
    /// Taken in whole.
    Done(FileVerifier),
    /// The bytes do not decode as a manifest. Kept, so packs report it
    /// without reading the manifest again, until the generation moves or
    /// the reuse window passes.
    Failed(String),
}

impl std::fmt::Debug for ManifestLoad {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match &self.stage {
            LoadStage::Reading { .. } => "reading",
            LoadStage::Decoding { .. } => "decoding",
            LoadStage::Unchanged => "unchanged",
            LoadStage::Done(_) => "done",
            LoadStage::Failed(_) => "failed",
        };
        formatter
            .debug_struct("ManifestLoad")
            .field("generation", &self.generation)
            .field("bytes", &self.bytes.len())
            .field("stage", &stage)
            .finish_non_exhaustive()
    }
}

impl ManifestLoad {
    /// Opens generation `generation`'s manifest. Called holding the
    /// maintenance lock, and only for the live generation.
    fn open(directory: &Path, generation: u64, now: Instant) -> Result<Self> {
        let file = File::open(generation::manifest_path(directory, generation))?;
        Ok(Self {
            generation,
            opened: now,
            bytes: Vec::new(),
            stage: LoadStage::Reading {
                file,
                hasher: payload::Sha256::new(),
            },
        })
    }

    /// The manifest did not decode.
    fn failed(&self) -> bool {
        matches!(self.stage, LoadStage::Failed(_))
    }

    /// A load that failed long enough ago to try again.
    fn stale_failure(&self, now: Instant) -> bool {
        self.failed() && now.saturating_duration_since(self.opened) > EVIDENCE_VERIFY_REUSE
    }

    /// Reads, digests and decodes, carrying over from `previous`, until the
    /// manifest is taken in or the budget is spent: reading checks the
    /// budget every [`FILE_CHECK_BYTES`], decoding every
    /// [`WALK_CHECK_INTERVAL`] values, so every call makes that much
    /// progress.
    fn advance(
        &mut self,
        previous: Option<&FileVerifier>,
        deadline: Option<crate::Deadline>,
    ) -> Result<Walked> {
        #[cfg(test)]
        probe::reading_manifest();
        let (mut spent, mut steps) = (0_u64, 0_u64);
        let mut chunk = Vec::new();
        loop {
            match &mut self.stage {
                LoadStage::Reading { file, hasher } => {
                    chunk.resize(64 * 1024, 0);
                    let read = file.read(&mut chunk)?;
                    if read == 0 {
                        let manifest_sha256 = hasher.clone().finalize_hex();
                        let same = previous.is_some_and(|previous| {
                            previous.generation == self.generation
                                && previous.manifest_sha256 == manifest_sha256
                        });
                        self.stage = match same {
                            true => LoadStage::Unchanged,
                            false => LoadStage::Decoding {
                                built: FileVerifier::empty(
                                    self.generation,
                                    manifest_sha256,
                                    self.opened,
                                ),
                                parse: ManifestParse::default(),
                            },
                        };
                        continue;
                    }
                    hasher.update(&chunk[..read]);
                    self.bytes.extend_from_slice(&chunk[..read]);
                    spent += read as u64;
                    if spent >= FILE_CHECK_BYTES {
                        spent = 0;
                        if out_of_budget(deadline) {
                            return Ok(Walked::OutOfBudget);
                        }
                    }
                }
                LoadStage::Decoding { built, parse } => {
                    match parse.step(&self.bytes, built, previous) {
                        Ok(false) => {}
                        Ok(true) => {
                            let stage = std::mem::replace(&mut self.stage, LoadStage::Unchanged);
                            if let LoadStage::Decoding { built, .. } = stage {
                                self.stage = LoadStage::Done(built);
                            }
                            // Decoded: the bytes are not needed again.
                            self.bytes = Vec::new();
                            return Ok(Walked::Reached);
                        }
                        Err(message) => {
                            self.bytes = Vec::new();
                            self.stage = LoadStage::Failed(message);
                            continue;
                        }
                    }
                    steps += 1;
                    if steps % WALK_CHECK_INTERVAL == 0 && out_of_budget(deadline) {
                        return Ok(Walked::OutOfBudget);
                    }
                }
                LoadStage::Unchanged | LoadStage::Done(_) => return Ok(Walked::Reached),
                LoadStage::Failed(message) => {
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "the manifest of generation {} does not decode: {message}",
                            self.generation
                        ),
                    )))
                }
            }
        }
    }

    /// The check this load produced, to replace `previous`, and the check it
    /// replaces, to be retired: `previous` itself and nothing when the
    /// manifest is the one it holds, else the new check — with the files
    /// `previous` holds open taken over — and `previous`. `previous` and
    /// nothing when the load is not finished.
    fn finish(
        self,
        previous: Option<FileVerifier>,
    ) -> (Option<FileVerifier>, Option<FileVerifier>) {
        match self.stage {
            LoadStage::Unchanged => (
                previous.map(|mut previous| {
                    previous.read_at = self.opened;
                    previous
                }),
                None,
            ),
            LoadStage::Done(mut built) => {
                let replaced = previous.map(|mut previous| {
                    built.adopt(&mut previous);
                    previous
                });
                (Some(built), replaced)
            }
            _ => (previous, None),
        }
    }

    /// How far it got, for a pack the budget refused.
    fn progress(&self) -> String {
        match &self.stage {
            LoadStage::Reading { .. } => format!(
                "{} bytes of generation {}'s manifest were read",
                self.bytes.len(),
                self.generation
            ),
            LoadStage::Decoding { built, .. } => format!(
                "{} entries of generation {}'s manifest ({} bytes) were taken in",
                built.files.len(),
                self.generation,
                self.bytes.len()
            ),
            _ => format!("generation {}'s manifest was taken in", self.generation),
        }
    }
}

/// The fields of a manifest, and which a manifest must have — what decoding
/// [`generation::Manifest`] whole demands.
const MANIFEST_FIELDS: [(&str, bool); 5] = [
    ("generation", true),
    ("created_unix_ns", true),
    ("folded_through", true),
    ("files", true),
    ("segment_format", false),
];

/// Where a [`ManifestLoad`] is in decoding a manifest: the byte it reached,
/// and which fields it has met.
///
/// A manifest is one JSON object whose `files` array holds an object per
/// file. This walks it a value at a time, each decoded by serde, so it can
/// stop between any two and resume there; and it accepts what decoding the
/// manifest whole does — every field once, the optional one at most once,
/// unknown fields ignored, nothing after the object.
#[derive(Debug, Default)]
struct ManifestParse {
    at: usize,
    stage: ParseStage,
    seen: [bool; MANIFEST_FIELDS.len()],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParseStage {
    /// Before the opening brace.
    #[default]
    Open,
    /// After it: a key, or the closing brace.
    First,
    /// A key, after a comma.
    Key,
    /// Inside `files`: a file's entry.
    File,
    /// After a field's value: a comma, or the closing brace.
    Next,
    /// After the closing brace.
    End,
}

impl ManifestParse {
    /// Takes one step — a key and its value, one file's entry, or a comma or
    /// brace — and returns whether the manifest is decoded. An error says
    /// what is wrong, and where.
    fn step(
        &mut self,
        bytes: &[u8],
        built: &mut FileVerifier,
        previous: Option<&FileVerifier>,
    ) -> std::result::Result<bool, String> {
        match self.stage {
            ParseStage::Open => {
                self.expect(bytes, b'{')?;
                self.stage = ParseStage::First;
            }
            ParseStage::First => {
                self.stage = match self.peek(bytes) {
                    Some(b'}') => {
                        self.at += 1;
                        ParseStage::End
                    }
                    _ => ParseStage::Key,
                }
            }
            ParseStage::Key => {
                let key: String = self.value(bytes)?;
                self.expect(bytes, b':')?;
                let field = MANIFEST_FIELDS.iter().position(|(name, _)| *name == key);
                if let Some(field) = field {
                    if std::mem::replace(&mut self.seen[field], true) {
                        return Err(format!("duplicate field `{key}` before byte {}", self.at));
                    }
                }
                self.stage = ParseStage::Next;
                match field.map(|field| MANIFEST_FIELDS[field].0) {
                    Some("generation" | "created_unix_ns") => {
                        self.value::<u64>(bytes)?;
                    }
                    Some("folded_through") => {
                        self.value::<generation::FoldedThrough>(bytes)?;
                    }
                    Some("files") => {
                        self.expect(bytes, b'[')?;
                        match self.peek(bytes) {
                            Some(b']') => self.at += 1,
                            _ => self.stage = ParseStage::File,
                        }
                    }
                    Some(_) => {
                        self.value::<Option<u16>>(bytes)?;
                    }
                    None => {
                        self.value::<serde::de::IgnoredAny>(bytes)?;
                    }
                }
            }
            ParseStage::File => {
                let entry: generation::ManifestFile = self.value(bytes)?;
                built.push(entry, previous);
                match self.peek(bytes) {
                    Some(b',') => self.at += 1,
                    Some(b']') => {
                        self.at += 1;
                        self.stage = ParseStage::Next;
                    }
                    _ => return Err(self.unexpected("`,` or `]`")),
                }
            }
            ParseStage::Next => {
                self.stage = match self.peek(bytes) {
                    Some(b',') => ParseStage::Key,
                    Some(b'}') => ParseStage::End,
                    _ => return Err(self.unexpected("`,` or `}`")),
                };
                self.at += 1;
            }
            ParseStage::End => {
                if self.peek(bytes).is_some() {
                    return Err(self.unexpected("the end of the manifest"));
                }
                let missing = MANIFEST_FIELDS
                    .iter()
                    .zip(self.seen)
                    .find(|((_, required), seen)| *required && !seen);
                if let Some(((name, _), _)) = missing {
                    return Err(format!("missing field `{name}`"));
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The next byte that is not JSON whitespace, without taking it.
    fn peek(&mut self, bytes: &[u8]) -> Option<u8> {
        while let Some(&byte) = bytes.get(self.at) {
            if !matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
                return Some(byte);
            }
            self.at += 1;
        }
        None
    }

    fn expect(&mut self, bytes: &[u8], wanted: u8) -> std::result::Result<(), String> {
        match self.peek(bytes) {
            Some(byte) if byte == wanted => {
                self.at += 1;
                Ok(())
            }
            _ => Err(self.unexpected(&format!("`{}`", char::from(wanted)))),
        }
    }

    fn unexpected(&self, wanted: &str) -> String {
        format!("expected {wanted} at byte {}", self.at)
    }

    /// Decodes the JSON value that starts here, and moves past it.
    fn value<T: serde::de::DeserializeOwned>(
        &mut self,
        bytes: &[u8],
    ) -> std::result::Result<T, String> {
        let rest = bytes.get(self.at..).unwrap_or_default();
        let mut values = serde_json::Deserializer::from_slice(rest).into_iter::<T>();
        match values.next() {
            Some(Ok(value)) => {
                self.at += values.byte_offset();
                Ok(value)
            }
            Some(Err(error)) => Err(format!("{error}, in the value at byte {}", self.at)),
            None => Err(format!("a value was expected at byte {}", self.at)),
        }
    }
}

/// `traza-server verify --ledger`: verifies a data directory no live server
/// owns, without opening it as a store.
///
/// Read-only on purpose. Opening the store would run recovery and the
/// ledger's own reconcile — appends, the very thing under examination — so
/// this takes the directory lock (refusing a directory a live server owns,
/// with [`Error::AlreadyOpen`]) and reads `CURRENT`, the manifest, the files,
/// the tombstone log and the ledger as they are. A store that crashed
/// between publishing a generation and ledgering it therefore reports the
/// live generation as not ledgered yet (and says so); its next open ledgers
/// it.
pub fn verify_offline(directory: &Path) -> Result<Verification> {
    verify_offline_as(directory, Detail::All)
}

/// [`verify_offline`], naming what it finds as `detail` says.
fn verify_offline_as(directory: &Path, detail: Detail<'_>) -> Result<Verification> {
    if !directory.join(generation::CURRENT_NAME).exists() {
        return Err(Error::Invalid(format!(
            "{} holds no published generation (no {}); is it a Traza data directory?",
            directory.display(),
            generation::CURRENT_NAME
        )));
    }
    let _lock = crate::DirectoryLock::acquire(directory.join(crate::LOCK_FILE_NAME))?;
    let live = generation::read_current(directory)?.ok_or_else(|| {
        Error::Invalid(format!(
            "{} holds no published generation",
            directory.display()
        ))
    })?;
    let started_at_ns = crate::unix_now_ns();
    let files = check_files(directory, live, started_at_ns)?;
    // The directory lock excludes every writer, so neither the chain nor the
    // tombstone log can move while they are read: the whole file is the
    // chain, the order of the reads does not matter, and no entry can be on
    // its way.
    let (statuses, tombstones_unreadable) = erasure::list_offline(directory)?;
    let settles = SettleIndex::from_statuses(&statuses);
    let facts = LiveFacts {
        generation: live,
        manifest_sha256: manifest_digest(directory, live)?,
        files,
        landing: HashSet::new(),
        tombstones_unreadable,
        failure: None,
        chain_verified_at_ns: started_at_ns,
    };
    let chain = verify_chain(&directory.join(LOG_NAME), None)?;
    let mut cross = CrossCheck::default();
    cross.sync(&chain, &settles, None);
    Ok(assemble(&chain, &cross, &settles, &facts, detail))
}

// ------------------------------------------------------------------- store

impl Store {
    /// Ledgers the generation just published, unless its manifest is the one
    /// already ledgered. Called by the checkpoint once `CURRENT` is durable.
    ///
    /// Never fails the checkpoint: the generation is committed whatever
    /// happens here (see the module docs, evidence not state). A failure is
    /// kept for verification to report, and the next checkpoint catches up —
    /// it ledgers its own generation and, while a failure is outstanding,
    /// every settled erasure the chain lacks, so a failed erasure entry waits
    /// one checkpoint rather than a restart.
    pub(crate) fn ledger_checkpoint(&self, generation: u64) {
        let epoch = self.ledger.failure_epoch();
        let catching_up = self.ledger.outstanding_failure().is_some();
        let outcome = (|| -> Result<()> {
            let digest = manifest_digest(&self.directory, generation)?;
            self.ledger.record_checkpoint(generation, digest)?;
            if catching_up {
                self.ledger.reconcile_erasures(&self.erasures.list()?)?;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => self.ledger.clear_failures_since(epoch),
            Err(error) => self
                .ledger
                .note_failure(&format!("ledgering generation {generation}"), &error),
        }
    }

    /// Ledgers an erasure just published by `generation`. Called once its
    /// settle record is durable, and — like [`Self::ledger_checkpoint`] —
    /// never fails it: the erasure is settled and its barrier lifted whatever
    /// happens here; a failure is kept and caught up at the next checkpoint.
    pub(crate) fn ledger_erasure(
        &self,
        erasure_id: u64,
        subject: &erasure::Subject,
        generation: u64,
    ) {
        if let Err(error) = self.ledger.record_erasure(erasure_id, subject, generation) {
            self.ledger
                .note_failure(&format!("ledgering erasure {erasure_id}"), &error);
        }
    }

    /// Marks erasure `erasure_id` as between its settle record and its
    /// entry until the guard drops; see [`Ledger::settling`]. Taken before
    /// the settle is written, dropped after [`Self::ledger_erasure`].
    pub(crate) fn ledger_settling(&self, erasure_id: u64) -> Settling<'_> {
        self.ledger.settling(erasure_id)
    }

    /// Runs the test hook between an erasure's settle and its entry.
    #[cfg(test)]
    pub(crate) fn ledger_after_settle_hook(&self) {
        self.ledger.run_hook(HookPoint::AfterSettle, self);
    }

    /// Copies the ledger into a pin of `generation` being built, beside its
    /// manifest: the chain through that generation's checkpoint entry, and
    /// nothing after it. Copies nothing when that entry's append failed —
    /// the pin is still a complete backup, and a copy ending elsewhere would
    /// misstate what it evidences.
    pub(crate) fn ledger_copy_into(&self, pin_dir: &Path, generation: u64) -> Result<()> {
        self.ledger
            .copy_through_checkpoint(generation, &pin_dir.join(LOG_NAME))
            .map(drop)
    }

    /// The chain's tip.
    pub fn ledger_head(&self) -> Result<LedgerHead> {
        self.ledger.head()
    }

    /// Up to `limit` entries with `seq` greater than `after` (all entries when
    /// `after` is `None`), in file order, exactly as stored. Lines that do not
    /// parse are skipped here; [`Self::verify_ledger`] names them.
    pub fn ledger_entries(&self, after: Option<u64>, limit: usize) -> Result<Vec<Value>> {
        self.ledger_page(after, limit).map(|(_, entries)| entries)
    }

    /// [`Self::ledger_entries`] together with the head they were read up to:
    /// one snapshot, so no entry in the page is past the head reported with
    /// it.
    pub fn ledger_page(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<(LedgerHead, Vec<Value>)> {
        let snapshot = self.ledger.snapshot()?;
        let mut entries = Vec::new();
        if limit > 0 {
            self.ledger.scan_prefix(snapshot.len, |entry| {
                let seq = entry.get("seq").and_then(Value::as_u64);
                if after.map_or(true, |after| seq.is_some_and(|seq| seq > after)) {
                    entries.push(entry);
                }
                entries.len() < limit
            })?;
        }
        Ok((snapshot.head, entries))
    }

    /// Verifies the whole chain, checks that the latest checkpoint entry
    /// names the live generation's manifest, re-digests every file that
    /// manifest names, checks every ledgered erasure still holds in the live
    /// tombstone log and every settled erasure there is ledgered, and
    /// reports any ledger append not yet caught up.
    ///
    /// The file check is O(store bytes), like [`Self::verify_generation`],
    /// and runs under the same maintenance lock: no compaction, expiry or
    /// checkpoint can move files or the chain's checkpoint entries
    /// mid-check. The chain is walked whole, from byte 0 and with no
    /// budget, AFTER that lock is released: it is read up to a snapshot
    /// taken under the lock, and nothing rewrites bytes inside that prefix,
    /// so the walk needs no lock at all — and erasure and anchor appends,
    /// which never took it, land past what is checked. Note that compaction
    /// and expiry replace files BETWEEN checkpoints, so the live manifest
    /// can name files that are gone until the next checkpoint publishes;
    /// those report as missing.
    ///
    /// Never reuses an earlier walk, file check or cross-check; its walk is
    /// offered to the evidence packs, which may extend it (see
    /// [`Self::evidence`]).
    pub fn verify_ledger(&self) -> Result<Verification> {
        let started = self.ledger.now();
        let started_at_ns = self.ledger.unix_now_ns();
        let (snapshot, live, manifest_sha256, files) = {
            let _maintenance = self.lock_maintenance()?;
            // Under the maintenance lock, so no checkpoint entry can land
            // between this and reading `CURRENT`; erasure and anchor appends
            // may, and the snapshot leaves them out.
            let snapshot = self.ledger.snapshot()?;
            let live = self.live_generation();
            let files = check_files(&self.directory, live, started_at_ns)?;
            // With the generation it names: once the lock is released, a
            // checkpoint may publish a successor and sweep this one away.
            let manifest_sha256 = manifest_digest(&self.directory, live)?;
            (snapshot, live, manifest_sha256, files)
        };
        #[cfg(test)]
        self.ledger.run_hook(HookPoint::AfterSnapshot, self);
        // Read after the snapshot: see `LiveFacts`.
        let mut settles = SettleIndex::default();
        settles.sync(&self.erasures, self.erasures.settled_count()?, None)?;
        let mut walk = ChainWalk::new();
        walk.advance(&self.ledger.path, Some(snapshot.len), None)?;
        let mut cross = CrossCheck::default();
        cross.sync(&walk, &settles, None);
        // After the settlements: see `Ledger::landing`.
        let landing = cross.landing(&self.ledger.landing(snapshot.len)?);
        let facts = LiveFacts {
            generation: live,
            manifest_sha256,
            files,
            landing,
            tombstones_unreadable: 0,
            failure: self.ledger.outstanding_failure(),
            chain_verified_at_ns: started_at_ns,
        };
        let verification = assemble(&walk, &cross, &settles, &facts, Detail::All);
        self.ledger.offer_walk(walk, started, started_at_ns)?;
        Ok(verification)
    }

    /// Appends an external notary's anchor, after checking its `target_head`
    /// is the head recorded at `target_seq` ([`Error::Conflict`] otherwise).
    pub fn anchor_ledger(&self, request: AnchorRequest) -> Result<Entry> {
        self.ledger.anchor(request)
    }

    /// The evidence pack for `tenant` (`None`: the whole store) over the
    /// window `[since_ns, until_ns]` (defaults: the epoch, now).
    ///
    /// Everything in it is something the store can already show — the chain
    /// and its verification, the retention policy, activity from the same
    /// deadline-bounded fold the analytics use, the erasures and their
    /// receipts — gathered into one document with a digest over its
    /// canonical JSON, which is what an external notary signs. A tenant's
    /// pack shows other tenants' erasure entries only as chain stubs (`seq`,
    /// `kind`, `at_ns`, `prev`, `head`, `redacted: true`), so the chain
    /// stays followable without disclosing whose erasure it was — the salt
    /// the stub leaves out is what keeps its head from being recomputed for
    /// a guessed subject.
    ///
    /// The head, the entries and the verification come from one snapshot of
    /// the chain, so the pack never reports a head older than an entry or a
    /// verification it carries.
    ///
    /// A tenant's pack carries that tenant's usage row and the retention
    /// window expiry actually applies to it; a whole-store pack carries the
    /// global window and no usage row, since it is nobody's account.
    ///
    /// **What a pack costs.** Bound (tenant) credentials can ask for one, so
    /// nothing in it may cost the whole chain, every erasure or the whole
    /// store per request, or hold off maintenance while it works. Packs keep
    /// what they established between requests ([`Ledger::evidence`]) and
    /// each pays for what changed:
    ///
    /// - ONE budget ([`crate::Config::query_deadline`], anchored here) for
    ///   every step — taking in settled erasures, the chain walk, the
    ///   erasure cross-check, the file check, the window lookups, the
    ///   activity fold and the receipts — so the request as a whole is
    ///   bounded, not each step afresh. A step past it stops where it can
    ///   resume, keeps its progress, and the pack is refused with
    ///   [`Error::DeadlineExceeded`] saying how far it got: a pack never
    ///   carries a partial verification. Retrying resumes it, so work longer
    ///   than one budget still finishes, a budget at a time — however long
    ///   that takes, since unfinished work is never discarded for its age.
    /// - The settled erasures are taken in from the tombstone log's settle
    ///   order once each — subject digested, indexed by settle time — so a
    ///   pack reads only the ones settled since the last.
    /// - The chain walk EXTENDS the last completed one — any pack's, or the
    ///   operator's — by what was appended since, and it runs after the
    ///   maintenance lock is released, under only the lock that serializes
    ///   packs. The erasure cross-check is kept with the walk and takes in
    ///   only the entries and settles new to it.
    /// - The window's entries, and the window's erasures, are found by
    ///   binary search in indexes by time; only those lines are read.
    /// - A new generation's manifest is taken in off the maintenance lock,
    ///   from a handle opened under it, a budget at a time ([`ManifestLoad`]).
    ///   Until a pack answers, packs answer for the snapshot and generation
    ///   that load began on, however many generations the store publishes
    ///   meanwhile, so the load finishes ([`Pin`]); the pack's head says
    ///   which snapshot that was.
    /// - The file check digests files off the maintenance lock, a budget at
    ///   a time, continuing a file it was part-way through; a new
    ///   generation re-digests only the files the checkpoint changed, and
    ///   an append-only log only what was appended. A problem found off the
    ///   lock is confirmed under it before it is reported — a batch of
    ///   [`CONFIRM_BATCH`] per hold, with the budget checked between holds —
    ///   and a pack whose generation the store published past mid-check
    ///   checks the new one. See [`FileVerifier`].
    /// - A file check replaced by the next generation's is freed a budget's
    ///   share at a time, never whole and never under the maintenance lock
    ///   ([`Evidence::reclaim`]).
    /// - The verification names at most [`EVIDENCE_MAX_PROBLEMS`] problems
    ///   of each kind its reader may see and counts the rest
    ///   (`problems_truncated`), from indexes kept with the cross-check, so
    ///   a store with many discrepancies costs a pack what it names.
    /// - A completed walk or file digest older than
    ///   [`EVIDENCE_VERIFY_REUSE`] is still served, with its age stated
    ///   (`chain_verified_at_ns`, `verified_at_ns`), while each pack, once
    ///   assembled, spends up to [`EVIDENCE_REFRESH_SLICE`] of what its
    ///   budget has left redoing it: a fresh walk from byte 0 (served once
    ///   complete), and the oldest file digests.
    pub fn evidence(
        &self,
        tenant: Option<&str>,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
    ) -> Result<Value> {
        let started = Instant::now();
        let deadline = self.query_deadline(started);
        let generated_at_ns = self.ledger.unix_now_ns();
        let since = since_ns.unwrap_or(0);
        let until = until_ns.unwrap_or(generated_at_ns);
        if since > until {
            return Err(Error::Invalid(format!(
                "since ({since}) is after until ({until})"
            )));
        }
        if let Some(tenant) = tenant {
            if !tenant.is_empty() && !crate::valid_tenant(tenant) {
                return Err(Error::Invalid(
                    "tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes".to_owned(),
                ));
            }
        }

        let chain = self.evidence_chain(tenant, since, until, started, deadline)?;
        let target = chain.target;
        let verification = chain.verification;
        let head = chain.head;
        let entries: Vec<Value> = self
            .ledger
            .read_lines_at(&chain.window)?
            .into_iter()
            .map(|entry| scope_entry(entry, tenant))
            .collect();

        let mut pack = json!({
            "format": EVIDENCE_FORMAT,
            "generated_at_ns": generated_at_ns,
            "engine_version": env!("CARGO_PKG_VERSION"),
            "store_id": head.store_id,
            "tenant": tenant,
            "window": {"since_ns": since, "until_ns": until},
            "ledger": {
                "head": head,
                "entries": entries,
                "entries_truncated": chain.window_truncated,
                "entries_unreadable": chain.unplaceable,
                "verification": verification,
            },
            "retention": self.retention_for(tenant)?,
            "usage": self.evidence_usage(tenant)?,
            "activity": self.evidence_activity(tenant, since, until, deadline)?,
            "erasures": self.evidence_erasures(&chain.erasures, deadline)?,
            "erasures_truncated": chain.erasures_truncated,
        });
        let digest = payload::sha256_hex(evals::canonical_json(&pack).as_bytes());
        if let Value::Object(map) = &mut pack {
            map.insert("digest".to_owned(), Value::String(digest));
        }
        // Answered: the next pack takes the store as it is then. Only now,
        // so a pack refused after its chain — the activity, the erasures —
        // is retried against the same pair rather than a newer one.
        {
            let mut state = self.ledger.lock_evidence()?;
            let answered = state
                .pin
                .as_ref()
                .is_some_and(|pin| (pin.snapshot.len, pin.generation) == target);
            if answered {
                state.pin = None;
            }
        }
        // Best effort, and after the pack is whole, so a redo can neither
        // fail the pack nor take budget its own steps needed. Anything it
        // trips over, the next pack's own steps meet and report.
        let _ = self.evidence_refresh(deadline);
        Ok(pack)
    }

    /// Everything in a pack that needs [`Ledger::evidence`] held. First, a
    /// share of freeing what earlier packs replaced ([`Evidence::reclaim`]).
    /// Then, under the maintenance lock, only the snapshot, the live
    /// generation and — when the file check is of another generation, or
    /// its manifest was read longer ago than the reuse window — opening the
    /// manifest; or, while an earlier pack's manifest load is unanswered,
    /// that pack's snapshot and generation instead ([`Pin`]). Then, outside
    /// it and under `deadline`, the settled erasures taken in, the walk, its
    /// cross-check, the manifest and the file check, each resuming where the
    /// last pack left it. Then, only when the file check found a problem
    /// off the lock, holds of it to confirm the problems, a batch per hold
    /// with the budget checked between (see [`FileVerifier`]); a
    /// confirmation that finds the store moved — another generation live,
    /// or a path holding another file — runs the steps again, from a new
    /// snapshot when the generation moved, each continuing from what is
    /// kept, at most [`EVIDENCE_MAX_ROUNDS`] times. Then the verification
    /// and the window lookups. See [`Self::evidence`] for why each step is
    /// shaped as it is.
    fn evidence_chain(
        &self,
        tenant: Option<&str>,
        since: u64,
        until: u64,
        started: Instant,
        deadline: Option<crate::Deadline>,
    ) -> Result<EvidenceChain> {
        let refused = |what: String| {
            Error::DeadlineExceeded(format!(
                "{} ms elapsed; {what} before the budget ran out. The check resumes from there \
                 on the next request, so retrying completes it; or raise --query-deadline-ms",
                started.elapsed().as_millis()
            ))
        };
        // Before the maintenance lock: see `Ledger::evidence`.
        let mut state = self.ledger.lock_evidence()?;
        let state = &mut *state;
        // Whatever is left is the next pack's to free: running out here
        // leaves this one's own steps their one interval each.
        state.reclaim(deadline);
        let mut round = 0_u32;
        let (snapshot, live, landing) = loop {
            round += 1;
            let now = self.ledger.now();
            state.release_stale_pin(now);
            let (snapshot, live, displaced) = match &state.pin {
                Some(pin) => (pin.snapshot.clone(), pin.generation, None),
                None => {
                    let _maintenance = self.lock_maintenance()?;
                    #[cfg(test)]
                    let _held = probe::holding_maintenance();
                    // Under the maintenance lock, so no checkpoint entry can
                    // land between this and reading `CURRENT`, and no sweep
                    // can remove the manifest before it is open; erasure and
                    // anchor appends may land, and the snapshot leaves them
                    // out.
                    let snapshot = self.ledger.snapshot()?;
                    let live = self.live_generation();
                    let displaced = state.open_manifest(&self.directory, live, now)?;
                    (snapshot, live, displaced)
                }
            };
            if let Some(displaced) = displaced {
                state.retire_load(displaced);
            }
            #[cfg(test)]
            self.ledger.run_hook(HookPoint::AfterSnapshot, self);

            // The settled erasures, read after the snapshot: see `LiveFacts`.
            let settled = self.erasures.settled_count()?;
            if state.settles.sync(&self.erasures, settled, deadline)? == Walked::OutOfBudget {
                return Err(refused(format!(
                    "{} of {settled} settled erasures were taken in for the ledger's erasure \
                     cross-check",
                    state.settles.entries.len()
                )));
            }

            // The walk: the served one, extended; or, before any walk has
            // completed, the first one, which must finish before a pack can
            // answer — however many requests and reuse windows that takes.
            if state
                .served
                .as_ref()
                .map_or(true, |served| served.walk.len > snapshot.len)
            {
                let refresh = state.refresh.get_or_insert_with(|| {
                    CachedWalk::new(snapshot.len, now, self.ledger.unix_now_ns())
                });
                let (to, walked) =
                    refresh
                        .walk
                        .advance(&self.ledger.path, Some(refresh.target), deadline)?;
                if walked == Walked::OutOfBudget {
                    return Err(refused(format!(
                        "the ledger chain was verified through line {} (byte {} of {to})",
                        refresh.walk.line_number, refresh.walk.len
                    )));
                }
                state.served = state.refresh.take();
            }
            let Some(served) = state.served.as_mut() else {
                return Err(Error::Invalid(
                    "no ledger walk to serve; this is a bug".to_owned(),
                ));
            };
            let (to, walked) =
                served
                    .walk
                    .advance(&self.ledger.path, Some(snapshot.len), deadline)?;
            if walked == Walked::OutOfBudget {
                return Err(refused(format!(
                    "the ledger chain was verified through line {} (byte {} of {to})",
                    served.walk.line_number, served.walk.len
                )));
            }
            if served.cross.sync(&served.walk, &state.settles, deadline) == Walked::OutOfBudget {
                return Err(refused(format!(
                    "the ledger's erasure cross-check took in {} of {} erasure entries and {} \
                     of {} settled erasures",
                    served.cross.entries,
                    served.walk.erasures.len(),
                    served.cross.settles,
                    state.settles.entries.len()
                )));
            }

            // The live generation's manifest, when the file check is not of
            // it yet: a pack answers for the generation it snapshotted, and
            // until one does, packs answer for that pair ([`Pin`]). And one
            // read again that does not decode is reported, as reading it
            // would report it, rather than answered past.
            let required = state
                .files
                .as_ref()
                .map_or(true, |files| files.generation != live);
            let failed = state.loading.as_ref().is_some_and(ManifestLoad::failed);
            if required && state.pin.is_none() {
                state.pin = Some(Pin {
                    snapshot: snapshot.clone(),
                    generation: live,
                    at: now,
                });
            }
            if required || failed {
                let loaded = state.load_manifest(deadline).inspect_err(|_| {
                    // Its manifest does not decode, or cannot be read: the
                    // next pack takes the generation live then.
                    state.pin = None;
                })?;
                if loaded == Walked::OutOfBudget {
                    let progress = state
                        .loading
                        .as_ref()
                        .map_or_else(String::new, ManifestLoad::progress);
                    return Err(refused(format!(
                        "{progress} (read from a handle kept open, so it keeps its progress)"
                    )));
                }
                // A load that outlasted the window, finished at last: the
                // pair it pinned is too old to answer for, and the store as
                // it is now inherits its file digests by unchanged entry.
                if state.release_stale_pin(self.ledger.now()) {
                    continue;
                }
            }
            let Some(files) = state
                .files
                .as_mut()
                .filter(|files| files.generation == live)
            else {
                return Err(Error::Invalid(
                    "no file check of the live generation; this is a bug".to_owned(),
                ));
            };
            if files.check_unchecked(&self.directory, deadline, &self.ledger)?
                == Walked::OutOfBudget
            {
                let (done, total) = files.progress();
                return Err(refused(format!(
                    "{done} of {total} files of generation {} were digested (a file part-way \
                     through keeps its progress)",
                    files.generation
                )));
            }
            if files.unconfirmed() {
                // A batch per hold, the budget checked between holds: the
                // problems confirmed so far stay confirmed, and the rest wait
                // for the next request.
                let mut confirmed = Confirmed::Held;
                while files.unconfirmed() {
                    let batch = {
                        let _maintenance = self.lock_maintenance()?;
                        #[cfg(test)]
                        let _held = probe::holding_maintenance();
                        files.confirm(&self.directory, self.live_generation(), CONFIRM_BATCH)?
                    };
                    if batch == Confirmed::Moved {
                        confirmed = batch;
                        break;
                    }
                    if batch == Confirmed::Changed {
                        confirmed = batch;
                    }
                    if files.unconfirmed() && out_of_budget(deadline) {
                        let remaining = files.provisional.len();
                        return Err(refused(format!(
                            "{remaining} file problem(s) found off the maintenance lock were \
                             still to be confirmed against the store as it is now"
                        )));
                    }
                }
                if confirmed == Confirmed::Moved {
                    // What it found missing may be what the store swept:
                    // the pinned pair cannot be answered for.
                    state.pin = None;
                }
                if confirmed != Confirmed::Held {
                    if round >= EVIDENCE_MAX_ROUNDS {
                        return Err(Error::Conflict(format!(
                            "the store published a new generation, or replaced a file being \
                             checked, during each of {round} rounds of this pack's file check \
                             (generation {}); what was checked is kept, so retrying continues",
                            files.generation
                        )));
                    }
                    continue;
                }
            }
            // After the settlements: see `Ledger::landing`.
            let landing = match state.served.as_ref() {
                Some(served) => served.cross.landing(&self.ledger.landing(snapshot.len)?),
                None => HashSet::new(),
            };
            break (snapshot, live, landing);
        };

        let (Some(served), Some(files)) = (state.served.as_ref(), state.files.as_ref()) else {
            return Err(Error::Invalid(
                "no ledger walk or file check to report; this is a bug".to_owned(),
            ));
        };
        let facts = LiveFacts {
            generation: live,
            manifest_sha256: files.manifest_sha256.clone(),
            files: files.result(self.ledger.unix_now_ns()),
            landing,
            tombstones_unreadable: 0,
            failure: self.ledger.outstanding_failure(),
            chain_verified_at_ns: served.started_at_ns,
        };
        let verification = assemble(
            &served.walk,
            &served.cross,
            &state.settles,
            &facts,
            Detail::Pack(tenant),
        );
        let (window, window_truncated) =
            served
                .walk
                .times
                .window(since, until, EVIDENCE_MAX_ENTRIES, |_| true, deadline)?;
        let unplaceable = served.walk.unplaceable;
        if served.walk.missing {
            // Nothing to extend: the next pack looks for the file afresh.
            state.served = None;
        }
        let (listed, erasures_truncated) =
            state
                .settles
                .window(tenant, since, until, EVIDENCE_MAX_ERASURES, deadline)?;
        let erasures = listed
            .into_iter()
            .map(|settle| ListedErasure {
                id: settle.id,
                subject_kind: settle.subject_kind,
                published_at_ns: settle.settled_unix_ns,
            })
            .collect();
        Ok(EvidenceChain {
            target: (snapshot.len, live),
            head: snapshot.head,
            verification,
            window,
            window_truncated,
            unplaceable,
            erasures,
            erasures_truncated,
        })
    }

    /// Redoes checks past [`EVIDENCE_VERIFY_REUSE`] with what is left of a
    /// pack's budget, at most [`EVIDENCE_REFRESH_SLICE`] of it: a walk from
    /// byte 0 toward a fixed target (started once the served walk is past
    /// the window, kept however long it takes, and served once it and its
    /// cross-check are complete), then the live manifest read again (opened
    /// by a pack, taken in once read whole), then the files whose last
    /// digest is past the window, oldest first. See [`Self::evidence`].
    fn evidence_refresh(&self, deadline: Option<crate::Deadline>) -> Result<()> {
        let slice = Some(crate::Deadline::capped(deadline, EVIDENCE_REFRESH_SLICE));
        let mut state = self.ledger.lock_evidence()?;
        let state = &mut *state;
        let now = self.ledger.now();
        let stale = state.served.as_ref().is_some_and(|served| {
            now.saturating_duration_since(served.started) > EVIDENCE_VERIFY_REUSE
        });
        if stale && state.refresh.is_none() {
            // Any snapshot is a prefix every later one extends — but none
            // past a pinned one, which packs answer for until one does.
            let mut target = self.ledger.snapshot()?.len;
            if let Some(pin) = &state.pin {
                target = target.min(pin.snapshot.len);
            }
            state.refresh = Some(CachedWalk::new(target, now, self.ledger.unix_now_ns()));
        }
        if let Some(refresh) = state.refresh.as_mut() {
            let (_, walked) =
                refresh
                    .walk
                    .advance(&self.ledger.path, Some(refresh.target), slice)?;
            if walked == Walked::Reached
                && refresh.cross.sync(&refresh.walk, &state.settles, slice) == Walked::Reached
            {
                let done = state.refresh.take();
                if done.as_ref().is_some_and(|done| !done.walk.missing) {
                    state.served = done;
                }
            }
        }
        // A manifest that does not decode is the next pack's to report; the
        // files are still due their redo meanwhile.
        let loaded = state.load_manifest(slice);
        if let Some(files) = state.files.as_mut() {
            files.recheck_due(&self.directory, slice, &self.ledger)?;
        }
        state.reclaim(slice);
        loaded.map(drop)
    }

    /// The retention window that applies to `tenant`, and the layer that
    /// decided it — the same resolution expiry acts on
    /// ([`Store::effective_ttl`]), so the pack states the policy in force
    /// rather than the one the flags alone describe: the tenant's runtime
    /// limits document (`limits`), a `--tenant-ttl` override (`flag`), the
    /// global window (`global`), or nothing configured (`none`).
    /// `ttl_seconds` is `null` whenever the effective policy keeps data
    /// forever — a `0` at any layer included. A store-wide pack reports the
    /// global window: per-tenant layers describe no single answer for it.
    fn retention_for(&self, tenant: Option<&str>) -> Result<Value> {
        let Some(tenant) = tenant else {
            return Ok(match self.config.ttl_seconds.filter(|ttl| *ttl > 0) {
                Some(ttl) => json!({"ttl_seconds": ttl, "source": "global"}),
                None => json!({"ttl_seconds": Value::Null, "source": "none"}),
            });
        };
        let effective = self.effective_ttl(tenant)?;
        Ok(json!({
            "ttl_seconds": effective.ttl_seconds,
            "source": effective.source,
        }))
    }

    /// The tenant's usage row — exactly what `GET /v1/usage?tenant=` serves,
    /// counters, limits and quota state — or `null` for a store-wide pack.
    /// O(1): the counters are in memory, and no segment is read.
    fn evidence_usage(&self, tenant: Option<&str>) -> Result<Value> {
        let Some(tenant) = tenant else {
            return Ok(Value::Null);
        };
        Ok(self
            .usage_report(Some(tenant))?
            .tenants
            .into_iter()
            .next()
            .and_then(|row| serde_json::to_value(row).ok())
            .unwrap_or(Value::Null))
    }

    /// Spans, traces, sessions and errors started in the window, with a
    /// per-UTC-day breakdown, from one fold under the pack's `deadline`.
    /// Distinct traces and sessions are counted by a 128-bit hash of
    /// `(tenant, id)`, which keeps a large window's memory at 16 bytes per
    /// distinct id.
    fn evidence_activity(
        &self,
        tenant: Option<&str>,
        since: u64,
        until: u64,
        deadline: Option<crate::Deadline>,
    ) -> Result<Value> {
        fn key(tenant: &str, id: &str) -> crate::hash::Hash128 {
            crate::hash::hash_attribute(tenant, id)
        }
        let filter = SpanFilter {
            tenant: tenant.map(str::to_owned),
            since_ns: Some(since),
            until_ns: Some(until),
            limit: None,
            ..SpanFilter::default()
        };
        let mut spans = 0_u64;
        let mut errors = 0_u64;
        let mut traces = HashSet::new();
        let mut sessions = HashSet::new();
        let mut by_day: BTreeMap<String, (u64, u64, f64)> = BTreeMap::new();
        self.fold_spans_under(&filter, deadline, |span| {
            spans += 1;
            traces.insert(key(&span.tenant, &span.trace_id));
            let facts = self.facts(span);
            if let Some(session) = &facts.session {
                sessions.insert(key(&span.tenant, session));
            }
            let day = by_day
                .entry(crate::analytics::day_bucket(span.start_time_ns))
                .or_default();
            day.0 += 1;
            if span.status == "error" {
                errors += 1;
                day.1 += 1;
            }
            // Non-finite cost is bad instrumentation; one NaN must not turn
            // the day's total into one.
            if let Some(cost) = facts.cost_usd.filter(|cost| cost.is_finite()) {
                day.2 += cost;
            }
        })?;
        let by_day: Vec<Value> = by_day
            .into_iter()
            .map(|(day, (spans, errors, cost_usd))| {
                json!({"day": day, "spans": spans, "errors": errors, "cost_usd": cost_usd})
            })
            .collect();
        Ok(json!({
            "spans": spans,
            "traces": traces.len(),
            "sessions": sessions.len(),
            "errors": errors,
            "by_day": by_day,
        }))
    }

    /// The erasures a pack lists — published (settled) in its window,
    /// visible to its tenant, found by [`SettleIndex::window`] — each with
    /// its receipt's verdict while the receipt budget lasts.
    ///
    /// `conclusive` and `result` are the receipt's own fields; both are
    /// `null` for an erasure past [`EVIDENCE_MAX_RECEIPTS`] or past the
    /// pack's `deadline`, which the pack states rather than guesses. A
    /// receipt already begun runs to its end: a partial receipt is worse
    /// than a slow one.
    fn evidence_erasures(
        &self,
        listed: &[ListedErasure],
        deadline: Option<crate::Deadline>,
    ) -> Result<Vec<Value>> {
        let mut receipts = 0_usize;
        let mut out = Vec::with_capacity(listed.len());
        for erasure in listed {
            let in_budget =
                receipts < EVIDENCE_MAX_RECEIPTS && crate::Deadline::check(deadline, 0).is_ok();
            let (conclusive, result) = match in_budget {
                true => {
                    receipts += 1;
                    let receipt = self.verify_erasure(erasure.id)?;
                    (
                        Value::Bool(receipt.conclusive),
                        Value::String(receipt.result),
                    )
                }
                false => (Value::Null, Value::Null),
            };
            out.push(json!({
                "id": erasure.id,
                "subject_kind": erasure.subject_kind,
                "published_at_ns": erasure.published_at_ns,
                "conclusive": conclusive,
                "result": result,
            }));
        }
        Ok(out)
    }
}

/// One stored entry as `tenant`'s evidence pack shows it: whole, unless it is
/// an erasure entry that is not the tenant's, which becomes a stub carrying
/// only what following the chain needs. The stub leaves out the `salt`, and
/// with it any way to recompute its head for a guessed subject.
fn scope_entry(entry: Value, tenant: Option<&str>) -> Value {
    let Some(tenant) = tenant else {
        return entry;
    };
    let foreign_erasure = entry.get("kind").and_then(Value::as_str) == Some("erasure")
        && entry.get("tenant").and_then(Value::as_str) != Some(tenant);
    if !foreign_erasure {
        return entry;
    }
    let mut stub = Map::new();
    for key in ["seq", "kind", "at_ns", "prev", "head"] {
        if let Some(value) = entry.get(key) {
            stub.insert(key.to_owned(), value.clone());
        }
    }
    stub.insert("redacted".to_owned(), Value::Bool(true));
    Value::Object(stub)
}

/// Opens the ledger for a store being opened and reconciles it with what is
/// durable: the live generation and every settled erasure. See the module
/// docs (crash consistency).
///
/// Fails only when the ledger cannot be read. A reconcile append that fails
/// is kept for verification to report, and the store opens: the generation
/// and erasures it would have recorded are committed already, and the next
/// checkpoint retries them.
pub(crate) fn open_reconciled(
    directory: &Path,
    live_generation: u64,
    erasures: &erasure::ErasureLog,
) -> Result<Ledger> {
    let ledger = Ledger::open(directory)?;
    let reconciled = (|| -> Result<()> {
        ledger.record_checkpoint(
            live_generation,
            manifest_digest(directory, live_generation)?,
        )?;
        ledger.reconcile_erasures(&erasures.list()?)
    })();
    if let Err(error) = reconciled {
        ledger.note_failure("reconciling at open", &error);
    }
    Ok(ledger)
}

// --------------------------------------------------------------------- http

/// Whether `path` is one of the routes [`serve`] answers.
pub fn handles(path: &str) -> bool {
    path == "/v1/ledger" || path.starts_with("/v1/ledger/") || path == "/v1/evidence"
}

/// Whether `path` is store-global operator surface: the full chain, its
/// verification (which names store files) and anchoring. Bound (tenant)
/// credentials are refused these; `/v1/ledger/head` and `/v1/evidence` stay
/// open to them.
pub fn operator_only(path: &str) -> bool {
    matches!(
        path,
        "/v1/ledger" | "/v1/ledger/verify" | "/v1/ledger/anchors"
    )
}

/// Whether the request needs the `admin` scope: appending an anchor writes
/// into the chain an auditor relies on, which is not something a credential
/// minted to write telemetry may do.
pub fn requires_admin(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path == "/v1/ledger/anchors"
}

/// Answers one ledger or evidence request as `(status, body)`.
///
/// The server has authenticated the caller and applied [`operator_only`] and
/// [`requires_admin`] before this runs; what remains here is the tenant
/// binding of the evidence pack. `query` is the decoded query pairs; unknown
/// parameters are refused, like every other route. An `Err` is the engine's
/// and maps through the server's usual error table (a deadline is `400`, a
/// conflict `409`).
pub fn serve(
    store: &Store,
    method: &str,
    path: &str,
    query: &[(String, String)],
    body: &[u8],
    principal: Option<&crate::auth::Principal>,
) -> Result<(u16, Value)> {
    let refuse = |message: String| Ok((400, json!({ "error": message })));
    let number = |name: &str, value: &str| -> std::result::Result<u64, String> {
        value
            .parse::<u64>()
            .map_err(|_| format!("{name} must be a non-negative integer"))
    };
    match (method, path) {
        ("GET", "/v1/ledger") => {
            let mut after = None;
            let mut limit = DEFAULT_PAGE;
            for (key, value) in query {
                match key.as_str() {
                    "after" => match number("after", value) {
                        Ok(seq) => after = Some(seq),
                        Err(message) => return refuse(message),
                    },
                    "limit" => match number("limit", value) {
                        Ok(n) if (1..=MAX_PAGE as u64).contains(&n) => limit = n as usize,
                        _ => return refuse(format!("limit must be between 1 and {MAX_PAGE}")),
                    },
                    other => return refuse(format!("unknown query parameter: {other}")),
                }
            }
            let (head, entries) = store.ledger_page(after, limit)?;
            Ok((
                200,
                json!({"entries": entries, "head": {"seq": head.seq, "head": head.head}}),
            ))
        }
        ("GET", "/v1/ledger/head") => {
            if let Some((key, _)) = query.first() {
                return refuse(format!("unknown query parameter: {key}"));
            }
            Ok((200, serde_json::to_value(store.ledger_head()?)?))
        }
        ("GET", "/v1/ledger/verify") => {
            if let Some((key, _)) = query.first() {
                return refuse(format!("unknown query parameter: {key}"));
            }
            Ok((200, serde_json::to_value(store.verify_ledger()?)?))
        }
        ("POST", "/v1/ledger/anchors") => {
            let request: AnchorRequest = match serde_json::from_slice(body) {
                Ok(request) => request,
                Err(error) => return refuse(format!("anchor body: {error}")),
            };
            Ok((201, serde_json::to_value(store.anchor_ledger(request)?)?))
        }
        ("GET", "/v1/evidence") => {
            let mut since = None;
            let mut until = None;
            let mut selected: Option<String> = None;
            for (key, value) in query {
                match key.as_str() {
                    "since" | "since_ns" => match number("since", value) {
                        Ok(ns) => since = Some(ns),
                        Err(message) => return refuse(message),
                    },
                    "until" | "until_ns" => match number("until", value) {
                        Ok(ns) => until = Some(ns),
                        Err(message) => return refuse(message),
                    },
                    // `tenant=` with an empty value names the default tenant;
                    // omitting it means the whole store (unbound only).
                    "tenant" => selected = Some(value.clone()),
                    other => return refuse(format!("unknown query parameter: {other}")),
                }
            }
            let tenant = match principal.and_then(|principal| principal.tenant.clone()) {
                None => selected,
                Some(bound) => match selected {
                    Some(named) if named != bound => {
                        return Ok((
                            403,
                            json!({"error": "this credential is bound to a different tenant",
                                   "required": "unbound"}),
                        ))
                    }
                    _ => Some(bound),
                },
            };
            Ok((200, store.evidence(tenant.as_deref(), since, until)?))
        }
        _ => Ok((404, json!({"error": "not found"}))),
    }
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
            "traza-ledger-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn digest(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn lines(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join(LOG_NAME))
            .expect("ledger")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_head_formula_is_pinned_byte_for_byte() {
        // The formula is format: an auditor's script reproduces it from the
        // docs, so it may never drift. Pinned against an independent
        // computation of sha256("traza-ledger/1\n" + 64 zeros + "\n" +
        // canonical JSON).
        let entry = json!({
            "seq": 0, "kind": "genesis", "at_ns": 1_700_000_000_000_000_000_u64,
            "prev": GENESIS_PREV, "store_id": "00112233445566778899aabbccddeeff",
            "head": "ignored",
        });
        let canonical = "{\"at_ns\":1700000000000000000,\"kind\":\"genesis\",\
                         \"prev\":\"0000000000000000000000000000000000000000000000000000000000000000\",\
                         \"seq\":0,\"store_id\":\"00112233445566778899aabbccddeeff\"}";
        let preimage = format!("traza-ledger/1\n{GENESIS_PREV}\n{canonical}");
        assert_eq!(
            compute_head(GENESIS_PREV, &entry),
            payload::sha256_hex(preimage.as_bytes())
        );
        assert_eq!(
            compute_head(GENESIS_PREV, &entry),
            "531d46b9dbcf1d123be71d3a20ca7538fe58e67f5879c348adbbc96b885c1cb9",
            "pinned: a change here re-baselines every ledger ever written"
        );
    }

    #[test]
    fn open_writes_one_genesis_and_appends_chain() {
        let dir = temp_dir("chain");
        let ledger = Ledger::open(&dir).expect("open");
        assert!(ledger
            .record_checkpoint(1, digest(1))
            .expect("cp")
            .is_some());
        assert!(
            ledger
                .record_checkpoint(1, digest(1))
                .expect("cp")
                .is_none(),
            "the same manifest is ledgered once"
        );
        assert!(ledger
            .record_checkpoint(2, digest(2))
            .expect("cp")
            .is_some());
        drop(ledger);

        // Reopening finds the genesis and does not write another.
        let ledger = Ledger::open(&dir).expect("reopen");
        let head = ledger.head().expect("head");
        assert_eq!(head.seq, 2);
        assert!(head.store_id.as_ref().is_some_and(|id| id.len() == 32));
        assert!(ledger
            .record_checkpoint(2, digest(2))
            .expect("cp")
            .is_none());

        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(report.problems().is_empty(), "{:?}", report.problems());
        assert_eq!(report.entries, 3);
        assert_eq!(report.head.as_deref(), Some(head.head.as_str()));
        assert_eq!(report.last_checkpoint, Some((2, 2, digest(2))));
    }

    #[test]
    fn two_stores_get_different_ids() {
        let a = Ledger::open(&temp_dir("id-a"))
            .expect("a")
            .head()
            .expect("head");
        let b = Ledger::open(&temp_dir("id-b"))
            .expect("b")
            .head()
            .expect("head");
        assert_ne!(a.store_id, b.store_id);
    }

    #[test]
    fn a_torn_final_append_is_healed_and_the_chain_continues() {
        let dir = temp_dir("torn");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(1, digest(1)).expect("cp");
        drop(ledger);
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(LOG_NAME))
            .expect("append");
        file.write_all(b"{\"seq\":2,\"kind\":\"checkp")
            .expect("torn");
        drop(file);

        let ledger = Ledger::open(&dir).expect("reopen heals");
        assert_eq!(ledger.head().expect("head").seq, 1);
        ledger.record_checkpoint(2, digest(2)).expect("cp");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(report.problems().is_empty(), "{:?}", report.problems());
        assert_eq!(report.entries, 3);
    }

    #[test]
    fn a_tampered_entry_is_named_by_seq_and_only_it() {
        let dir = temp_dir("tamper");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(1, digest(1)).expect("cp");
        ledger.record_checkpoint(2, digest(2)).expect("cp");
        drop(ledger);
        let mut all = lines(&dir);
        // Rewrite seq 1's generation: still valid JSON, still a checkpoint.
        all[1] = all[1].replace("\"generation\":1", "\"generation\":7");
        fs::write(dir.join(LOG_NAME), all.join("\n") + "\n").expect("write");

        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert_eq!(report.problems().len(), 1, "{:?}", report.problems());
        assert!(report.problems()[0].starts_with("seq 1: head does not match"));

        // A store opened over the damage keeps working: it appends after the
        // last readable entry, and the damage stays reported.
        let ledger = Ledger::open(&dir).expect("open over damage");
        ledger.record_checkpoint(3, digest(3)).expect("cp");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert_eq!(report.problems().len(), 1, "{:?}", report.problems());
    }

    #[test]
    fn an_added_field_breaks_the_head_even_though_the_entry_still_decodes() {
        let dir = temp_dir("added");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(1, digest(1)).expect("cp");
        drop(ledger);
        let mut all = lines(&dir);
        all[1] = all[1].replacen('{', "{\"note\":\"hello\",", 1);
        fs::write(dir.join(LOG_NAME), all.join("\n") + "\n").expect("write");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(
            report
                .problems()
                .iter()
                .any(|p| p.starts_with("seq 1: head")),
            "{:?}",
            report.problems()
        );
    }

    #[test]
    fn a_deleted_entry_breaks_the_link_after_it() {
        let dir = temp_dir("deleted");
        let ledger = Ledger::open(&dir).expect("open");
        for n in 1..=3 {
            ledger.record_checkpoint(n, digest(n as u8)).expect("cp");
        }
        drop(ledger);
        let mut all = lines(&dir);
        all.remove(2);
        fs::write(dir.join(LOG_NAME), all.join("\n") + "\n").expect("write");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(
            report
                .problems()
                .iter()
                .any(|p| p.starts_with("seq 3 (line 3): out of order")),
            "{:?}",
            report.problems()
        );
    }

    #[test]
    fn anchors_must_name_a_real_head() {
        let dir = temp_dir("anchor");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(1, digest(1)).expect("cp");
        let genesis_head = lines(&dir)[0].clone();
        let genesis_head: Entry = serde_json::from_str(&genesis_head).expect("entry");
        let request = |target_seq: u64, target_head: &str| AnchorRequest {
            target_seq,
            target_head: target_head.to_owned(),
            authority: "notary.example".to_owned(),
            anchored_at_ns: 42,
            signature: "c2ln".to_owned(),
            public_key: "a2V5".to_owned(),
            note: None,
        };
        assert!(matches!(
            ledger.anchor(request(0, &digest(9))),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            ledger.anchor(request(5, &genesis_head.head)),
            Err(Error::Conflict(_))
        ));
        assert!(
            matches!(
                ledger.anchor(request(0, "not-hex")),
                Err(Error::Conflict(_))
            ),
            "a malformed head cannot be the recorded one: a conflict, like any other"
        );
        let mut empty = request(0, &genesis_head.head);
        empty.signature.clear();
        assert!(matches!(ledger.anchor(empty), Err(Error::Invalid(_))));

        let anchored = ledger
            .anchor(request(0, &genesis_head.head))
            .expect("a matching anchor is appended");
        assert_eq!(anchored.seq, 2);
        assert_eq!(anchored.body.kind(), "anchor");
        // The tip itself is anchorable without a file scan.
        let tip = ledger.head().expect("head");
        ledger
            .anchor(request(tip.seq, &tip.head))
            .expect("anchor the tip");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(report.problems().is_empty(), "{:?}", report.problems());
    }

    #[test]
    fn erasure_entries_carry_a_digest_never_the_subject() {
        let dir = temp_dir("erasure");
        let ledger = Ledger::open(&dir).expect("open");
        let subject = erasure::Subject::Session {
            session_id: "alice@example.com".to_owned(),
            tenant: "acme".to_owned(),
        };
        let entry = ledger
            .record_erasure(3, &subject, 9)
            .expect("record")
            .expect("appended");
        assert!(ledger
            .record_erasure(3, &subject, 9)
            .expect("again")
            .is_none());
        let text = fs::read_to_string(dir.join(LOG_NAME)).expect("read");
        assert!(!text.contains("alice"), "{text}");
        let canonical =
            "{\"kind\":\"session\",\"session_id\":\"alice@example.com\",\"tenant\":\"acme\"}";
        match entry.body {
            Body::Erasure {
                erasure_id,
                tenant,
                subject_kind,
                subject_digest,
                generation,
                salt,
            } => {
                assert_eq!(erasure_id, 3);
                assert_eq!(tenant.as_deref(), Some("acme"));
                assert_eq!(subject_kind, "session");
                assert_eq!(subject_digest, payload::sha256_hex(canonical.as_bytes()));
                assert_eq!(generation, 9);
                assert_eq!(salt.len(), 32);
                assert!(salt.bytes().all(|byte| byte.is_ascii_hexdigit()));
            }
            other => panic!("not an erasure entry: {other:?}"),
        }
        // A payload subject is store-global: its tenant is null.
        let payload_subject = erasure::Subject::Payload {
            reference: format!("sha256/{}", digest(7)),
        };
        let entry = ledger
            .record_erasure(4, &payload_subject, 9)
            .expect("record")
            .expect("appended");
        let value = serde_json::to_value(&entry).expect("json");
        assert_eq!(value["tenant"], Value::Null);
    }

    #[test]
    fn scoped_entries_stub_foreign_erasures_and_keep_the_links() {
        let own = json!({"seq": 3, "kind": "erasure", "at_ns": 1, "prev": "p", "head": "h",
                         "tenant": "acme", "erasure_id": 1, "subject_kind": "trace",
                         "subject_digest": "d"});
        let foreign = json!({"seq": 4, "kind": "erasure", "at_ns": 2, "prev": "h", "head": "i",
                             "tenant": "bigco", "erasure_id": 2, "subject_kind": "tenant",
                             "subject_digest": "e", "generation": 5, "salt": "s"});
        assert_eq!(scope_entry(own.clone(), Some("acme")), own);
        assert_eq!(scope_entry(foreign.clone(), None), foreign);
        let stub = scope_entry(foreign, Some("acme"));
        assert_eq!(
            stub,
            json!({"seq": 4, "kind": "erasure", "at_ns": 2, "prev": "h", "head": "i",
                   "redacted": true})
        );
    }

    fn kinds(store: &Store) -> Vec<String> {
        store
            .ledger_entries(None, MAX_PAGE)
            .expect("entries")
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    #[test]
    fn open_ledgers_the_generation_and_erasure_a_crash_left_unledgered() {
        let dir = temp_dir("reconcile");
        let span: crate::Span = serde_json::from_value(json!({
            "trace_id": "t1", "span_id": "s1", "name": "chat", "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        }))
        .expect("span");
        {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            assert_eq!(kinds(&store), ["genesis", "checkpoint"]);
            store.ingest(span).expect("ingest");
            store
                .erase(erasure::Subject::Trace {
                    trace_id: "t1".to_owned(),
                    tenant: String::new(),
                })
                .expect("erase");
            store.checkpoint().expect("checkpoint");
            assert_eq!(
                kinds(&store),
                [
                    "genesis",
                    "checkpoint",
                    "checkpoint",
                    "erasure",
                    "checkpoint"
                ],
                "the erasure's own checkpoint, then the erasure, then the next checkpoint"
            );
        }
        // A crash after the settle and after the final CURRENT rename, but
        // before either ledger append: both facts are durable, neither entry.
        let all = lines(&dir);
        fs::write(dir.join(LOG_NAME), all[..3].join("\n") + "\n").expect("truncate");

        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        assert_eq!(
            kinds(&store),
            [
                "genesis",
                "checkpoint",
                "checkpoint",
                "checkpoint",
                "erasure"
            ]
        );
        let entries = store.ledger_entries(Some(2), MAX_PAGE).expect("entries");
        assert_eq!(entries[0]["generation"], json!(store.live_generation()));
        assert_eq!(entries[1]["erasure_id"], json!(1));
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        drop(store);

        // Reconcile is idempotent: a clean reopen appends nothing.
        let store = Store::open(&dir, crate::Config::default()).expect("reopen again");
        assert_eq!(kinds(&store).len(), 5);
    }

    #[test]
    fn a_checkpoint_that_publishes_is_ledgered_and_matches_the_live_manifest() {
        let dir = temp_dir("store");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        let generation = store.checkpoint().expect("checkpoint");
        let head = store.ledger_head().expect("head");
        let tip = store.ledger_entries(Some(head.seq - 1), 1).expect("tip");
        assert_eq!(tip[0]["kind"], "checkpoint");
        assert_eq!(tip[0]["generation"], json!(generation));
        assert_eq!(
            tip[0]["manifest_sha256"],
            json!(manifest_digest(&dir, generation).expect("digest"))
        );
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        assert!(verification.generation.matches_ledger);
        assert!(verification.generation.files_verified);
    }

    fn test_span(trace: &str, tenant: &str) -> crate::Span {
        serde_json::from_value(json!({
            "trace_id": trace, "span_id": "s1", "name": "chat", "service": "svc",
            "start_time_ns": 1_000u64, "end_time_ns": 2_000u64, "$tenant": tenant,
        }))
        .expect("span")
    }

    fn trace_subject(trace: &str, tenant: &str) -> erasure::Subject {
        erasure::Subject::Trace {
            trace_id: trace.to_owned(),
            tenant: tenant.to_owned(),
        }
    }

    fn anchor_request(target_seq: u64, target_head: &str, note: Option<String>) -> AnchorRequest {
        AnchorRequest {
            target_seq,
            target_head: target_head.to_owned(),
            authority: "notary.example".to_owned(),
            anchored_at_ns: 42,
            signature: "c2ln".to_owned(),
            public_key: "a2V5".to_owned(),
            note,
        }
    }

    #[test]
    fn a_foreign_stub_cannot_be_matched_to_a_guessed_tenant() {
        // What a bound tenant holds about a neighbour's erasure: the stub in
        // its evidence pack — seq, at_ns, prev, head — plus everything that
        // is guessable: small erasure ids and generations, five subject
        // kinds, and candidate tenant names (a tenant subject's digest
        // depends on nothing else). It tries them all against the stub's
        // head.
        let dir = temp_dir("bruteforce");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(1, digest(1)).expect("cp");
        let erased = erasure::Subject::Tenant {
            tenant: "bigco".to_owned(),
        };
        let entry = ledger
            .record_erasure(7, &erased, 2)
            .expect("record")
            .expect("appended");
        let full = serde_json::to_value(&entry).expect("json");
        let stub = scope_entry(full.clone(), Some("acme"));
        assert!(stub.get("salt").is_none(), "{stub}");
        let prev = stub["prev"].as_str().expect("prev");
        let guess = |salt: Option<&str>| -> Option<String> {
            for tenant in ["acme", "bigco", "globex", "initech"] {
                let subject = erasure::Subject::Tenant {
                    tenant: tenant.to_owned(),
                };
                for erasure_id in 0..=16_u64 {
                    for generation in 0..=8_u64 {
                        for kind in ["trace", "span", "session", "payload", "tenant"] {
                            let mut attempt = json!({
                                "seq": stub["seq"], "kind": "erasure", "at_ns": stub["at_ns"],
                                "prev": prev, "erasure_id": erasure_id, "tenant": tenant,
                                "subject_kind": kind, "subject_digest": subject_digest(&subject),
                                "generation": generation,
                            });
                            if let Some(salt) = salt {
                                attempt["salt"] = json!(salt);
                            }
                            if compute_head(prev, &attempt) == stub["head"] {
                                return Some(tenant.to_owned());
                            }
                        }
                    }
                }
            }
            None
        };
        // The search is sound: given the salt, which only the full entry
        // carries, it names the erased tenant...
        assert_eq!(guess(full["salt"].as_str()).as_deref(), Some("bigco"));
        // ...and from the stub alone it names nobody.
        assert_eq!(guess(None), None);
    }

    #[test]
    fn a_rollback_restore_is_flagged_and_re_erasing_is_ledgered_again() {
        let dir = temp_dir("rollback");
        {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            store.ingest(test_span("a1", "acme")).expect("ingest");
            store.ingest(test_span("b1", "bigco")).expect("ingest");
            store.pin_generation("before").expect("pin");
            store.erase(trace_subject("a1", "acme")).expect("erase");
            store.erase(trace_subject("b1", "bigco")).expect("erase");
            let verification = store.verify_ledger().expect("verify");
            assert!(verification.intact, "{verification:?}");
        }

        // Rewind: the pin from before both erasures, restored in place. The
        // tombstone log goes back with it and the data comes back; the
        // ledger stays, as it must — and still records both erasures.
        let store = Store::restore(&dir, dir.join("pins/before"), crate::Config::default())
            .expect("restore");
        let verification = store.verify_ledger().expect("verify");
        assert!(!verification.intact);
        assert!(
            verification.generation.matches_ledger,
            "the install is ledgered"
        );
        let undone = |problems: &[String]| {
            problems
                .iter()
                .filter(|problem| problem.contains("holds no settled erasure"))
                .count()
        };
        assert_eq!(
            undone(&verification.problems),
            2,
            "{:?}",
            verification.problems
        );
        // acme's pack keeps acme's finding and withholds bigco's, saying
        // only that something outside the tenant was withheld.
        let pack = store
            .evidence(Some("acme"), None, None)
            .expect("acme's pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(false));
        let scoped: Vec<String> =
            serde_json::from_value(pack["ledger"]["verification"]["problems"].clone())
                .expect("problems");
        assert_eq!(undone(&scoped), 1, "{scoped:?}");
        assert!(
            scoped
                .iter()
                .any(|problem| problem.starts_with("1 problem(s) about data outside this tenant")),
            "{scoped:?}"
        );
        assert!(!format!("{scoped:?}").contains("bigco"));

        // Re-applying them: the rewound log reuses the ids, and the subjects
        // are the same — but each is a new publication, and is ledgered.
        let again = store.erase(trace_subject("a1", "acme")).expect("re-erase");
        store.erase(trace_subject("b1", "bigco")).expect("re-erase");
        let acme: Vec<Value> = store
            .ledger_entries(None, MAX_PAGE)
            .expect("entries")
            .into_iter()
            .filter(|entry| entry["kind"] == "erasure" && entry["tenant"] == "acme")
            .collect();
        assert_eq!(acme.len(), 2, "{acme:?}");
        assert_eq!(
            acme[0]["erasure_id"], acme[1]["erasure_id"],
            "the id is reused"
        );
        assert_eq!(acme[0]["subject_digest"], acme[1]["subject_digest"]);
        assert_ne!(acme[0]["generation"], acme[1]["generation"]);
        assert_eq!(
            acme[1]["generation"],
            json!(again.settle.as_ref().expect("settled").generation)
        );
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        drop(store);

        // The reconcile recognizes each publication: a reopen appends none.
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        assert_eq!(
            kinds(&store)
                .iter()
                .filter(|kind| *kind == "erasure")
                .count(),
            4
        );
    }

    #[test]
    fn an_older_erasure_does_not_vouch_for_a_newer_one_a_restore_undid() {
        // The same subject erased twice, with a pin in between: restoring
        // that pin undoes the SECOND erasure only. The first is still
        // settled in the restored tombstone log, and the subject is the
        // same — but it published before the data the second one removed
        // was even ingested, so it cannot stand in for it.
        let dir = temp_dir("repeat");
        let later_span = || -> crate::Span {
            serde_json::from_value(json!({
                "trace_id": "t1", "span_id": "s2", "name": "chat", "service": "svc",
                "start_time_ns": 3_000u64, "end_time_ns": 4_000u64, "$tenant": "acme",
            }))
            .expect("span")
        };
        let second = {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            store.ingest(test_span("t1", "acme")).expect("ingest");
            store.erase(trace_subject("t1", "acme")).expect("erase");
            store
                .ingest(later_span())
                .expect("ingest after the erasure");
            store.pin_generation("between").expect("pin");
            let second = store.erase(trace_subject("t1", "acme")).expect("erase");
            let verification = store.verify_ledger().expect("verify");
            assert!(verification.intact, "{verification:?}");
            second
        };

        let store = Store::restore(&dir, dir.join("pins/between"), crate::Config::default())
            .expect("restore");
        assert_eq!(
            store.get_trace_in(Some("acme"), "t1").expect("trace").len(),
            1,
            "the data the second erasure removed is back"
        );
        let verification = store.verify_ledger().expect("verify");
        assert!(!verification.intact, "{verification:?}");
        let second_seq = store
            .ledger_entries(None, MAX_PAGE)
            .expect("entries")
            .into_iter()
            .find(|entry| {
                entry["kind"] == "erasure"
                    && entry["generation"]
                        == json!(second.settle.as_ref().expect("settled").generation)
            })
            .and_then(|entry| entry["seq"].as_u64())
            .expect("the second erasure's entry");
        let undone: Vec<&String> = verification
            .problems
            .iter()
            .filter(|problem| problem.contains("holds no settled erasure"))
            .collect();
        assert_eq!(undone.len(), 1, "{:?}", verification.problems);
        assert!(
            undone[0].starts_with(&format!("seq {second_seq}: erasure 2 ")),
            "{undone:?}"
        );

        // Re-applied, it holds again, and a reopen re-ledgers nothing.
        store.erase(trace_subject("t1", "acme")).expect("re-erase");
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        let erasures = |store: &Store| {
            kinds(store)
                .iter()
                .filter(|kind| *kind == "erasure")
                .count()
        };
        assert_eq!(erasures(&store), 3);
        drop(store);
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        assert_eq!(erasures(&store), 3);
        assert!(store.verify_ledger().expect("verify").intact);
    }

    #[test]
    fn a_settled_erasure_missing_from_the_chain_is_named() {
        // The crash window between an erasure's settle record and its
        // ledger entry, as the files show it: everything durable, the entry
        // absent, and the checkpoint entry before it matching CURRENT — so
        // nothing but the tombstone log itself says an entry is missing.
        let dir = temp_dir("unledgered");
        let erased = {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            store.ingest(test_span("t1", "acme")).expect("ingest");
            store.erase(trace_subject("t1", "acme")).expect("erase")
        };
        let all = lines(&dir);
        assert!(all[all.len() - 1].contains("\"kind\":\"erasure\""));
        fs::write(dir.join(LOG_NAME), all[..all.len() - 1].join("\n") + "\n").expect("truncate");

        let verification = verify_offline(&dir).expect("verify");
        assert!(!verification.intact, "{verification:?}");
        assert!(verification.generation.matches_ledger);
        let missing: Vec<&String> = verification
            .problems
            .iter()
            .filter(|problem| problem.contains("is not in the ledger"))
            .collect();
        assert_eq!(missing.len(), 1, "{:?}", verification.problems);
        let settled_in = erased.settle.as_ref().expect("settled").generation;
        assert!(
            missing[0].starts_with(&format!(
                "erasure {} (trace subject, settled in generation {settled_in})",
                erased.erase.id
            )),
            "{missing:?}"
        );
        // It is acme's erasure: acme's pack names it, another tenant's
        // withholds it.
        let named = |tenant| {
            verify_offline_as(&dir, Detail::Pack(Some(tenant)))
                .expect("verify")
                .problems
        };
        assert!(format!("{:?}", named("acme")).contains("is not in the ledger"));
        let bigco = named("bigco");
        assert!(
            !format!("{bigco:?}").contains("is not in the ledger"),
            "{bigco:?}"
        );
        assert!(
            bigco
                .iter()
                .any(|problem| problem.starts_with("1 problem(s) about data outside this tenant")),
            "{bigco:?}"
        );

        // Opening the store ledgers it, and the verdict is clean again.
        drop(Store::open(&dir, crate::Config::default()).expect("reopen"));
        let verification = verify_offline(&dir).expect("verify");
        assert!(verification.intact, "{verification:?}");
    }

    fn set_hook(store: &Store, point: HookPoint, hook: Hook) {
        let mut hooks = store.ledger.hooks.lock().expect("hooks");
        match point {
            HookPoint::AfterSettle => hooks.after_settle = Some(hook),
            HookPoint::AfterSnapshot => hooks.after_snapshot = Some(hook),
        }
    }

    #[test]
    fn a_verification_between_a_settle_and_its_entry_does_not_call_the_entry_missing() {
        // Live, the settle record is durable a moment before the entry is
        // appended; a verification landing in that moment must not report
        // the entry missing, as it would for one a crash lost.
        let dir = temp_dir("settle-window");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        store.ingest(test_span("t1", "acme")).expect("ingest");
        let seen = std::sync::Arc::new(Mutex::new(None));
        let sink = std::sync::Arc::clone(&seen);
        set_hook(
            &store,
            HookPoint::AfterSettle,
            Box::new(move |store: &Store| {
                let settled = store.erasures().expect("erasures")[0].settle.is_some();
                let verification = store.verify_ledger().expect("verify");
                *sink.lock().expect("sink") = Some((settled, verification));
            }),
        );
        store.erase(trace_subject("t1", "acme")).expect("erase");
        let (settled, verification) = seen.lock().expect("seen").take().expect("hook ran");
        assert!(settled, "the hook ran after the settle record");
        assert!(verification.intact, "{verification:?}");
        assert!(store.verify_ledger().expect("verify").intact);
    }

    #[test]
    fn an_entry_appended_after_the_snapshot_is_not_called_missing() {
        // The other half of the window: the settle and the entry both land
        // between a verification's snapshot and its read of the tombstone
        // log. The entry is past the prefix the verification reads, so the
        // prefix lacks it — and it is not missing.
        let dir = temp_dir("settle-after-snapshot");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        store.ingest(test_span("t1", "acme")).expect("ingest");
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        set_hook(
            &store,
            HookPoint::AfterSettle,
            Box::new(move |_: &Store| {
                parked_tx.send(()).expect("parked");
                release_rx.recv().expect("released");
            }),
        );
        std::thread::scope(|scope| {
            scope.spawn(|| {
                store.erase(trace_subject("t1", "acme")).expect("erase");
                done_tx.send(()).expect("done");
            });
            // The erasure is parked between its settle and its entry, its
            // checkpoint done and no lock held. Verify, and let it finish
            // after the snapshot and before the tombstone log is read.
            parked_rx.recv().expect("parked");
            let before = store.ledger_head().expect("head");
            set_hook(
                &store,
                HookPoint::AfterSnapshot,
                Box::new(move |_: &Store| {
                    release_tx.send(()).expect("release");
                    done_rx.recv().expect("erasure finished");
                }),
            );
            let verification = store.verify_ledger().expect("verify");
            assert_eq!(
                verification.head.as_deref(),
                Some(before.head.as_str()),
                "the entry is past the snapshot"
            );
            assert!(verification.intact, "{verification:?}");
        });
        assert!(store.verify_ledger().expect("verify").intact);
    }

    #[test]
    fn a_failed_append_never_fails_what_it_records_and_the_next_checkpoint_catches_up() {
        let dir = temp_dir("failing");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        store.ingest(test_span("t1", "")).expect("ingest");
        let path = dir.join(LOG_NAME);
        let saved = fs::read(&path).expect("ledger");
        // A ledger nobody can append to — on every platform, as root too: a
        // directory where the file was.
        fs::remove_file(&path).expect("remove");
        fs::create_dir(&path).expect("a directory in its place");

        let published = store
            .checkpoint()
            .expect("a committed checkpoint is reported as committed");
        assert_eq!(store.live_generation(), published);
        let erased = store
            .erase(trace_subject("t1", ""))
            .expect("the erasure settles");
        let settle = erased.settle.clone().expect("settled");
        assert!(
            store
                .erasures()
                .expect("erasures")
                .iter()
                .all(|status| status.settle.is_some()),
            "nothing is left pending, so no barrier keeps dropping spans"
        );
        store
            .pin_generation("during")
            .expect("a pin is a backup, ledger or not");
        assert!(
            !dir.join("pins/during").join(LOG_NAME).exists(),
            "no copy rather than one that does not end on the pinned checkpoint"
        );

        // Appendable again. Until the ledger catches up, verification says
        // what is missing and why.
        fs::remove_dir(&path).expect("rmdir");
        fs::write(&path, &saved).expect("put the ledger back");
        let verification = store.verify_ledger().expect("verify");
        assert!(!verification.intact);
        assert!(!verification.generation.matches_ledger);
        let problems = format!("{:?}", verification.problems);
        assert!(problems.contains("ledger append(s) failed"), "{problems}");
        assert!(
            problems.contains("is newer and not ledgered yet"),
            "{problems}"
        );

        // The next checkpoint ledgers its generation and the erasure whose
        // entry failed, and the failure clears.
        store.checkpoint().expect("checkpoint");
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        let entries = store.ledger_entries(None, MAX_PAGE).expect("entries");
        let tail: Vec<&str> = entries[entries.len() - 2..]
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(tail, ["checkpoint", "erasure"]);
        let last = &entries[entries.len() - 1];
        assert_eq!(last["erasure_id"], json!(erased.erase.id));
        assert_eq!(last["generation"], json!(settle.generation));
    }

    #[cfg(unix)]
    #[test]
    fn a_ledger_open_cannot_append_to_does_not_stop_the_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("readonly");
        {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            store.ingest(test_span("t1", "")).expect("ingest");
            store.checkpoint().expect("checkpoint");
        }
        // The last checkpoint's entry lost, as a crash before its append
        // loses it — so the reopen has something to ledger — and a ledger
        // the server may read but not write.
        let path = dir.join(LOG_NAME);
        let all = lines(&dir);
        fs::write(&path, all[..all.len() - 1].join("\n") + "\n").expect("truncate");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("chmod");
        if OpenOptions::new().append(true).open(&path).is_ok() {
            eprintln!(
                "SKIPPED: this user can write a read-only file (root?); \
                 the open-time append failure was not exercised"
            );
            return;
        }
        let store = Store::open(&dir, crate::Config::default())
            .expect("a ledger it cannot append to does not stop the store");
        let verification = store.verify_ledger().expect("verify");
        assert!(!verification.intact);
        assert!(
            verification
                .problems
                .iter()
                .any(|problem| problem.contains("reconciling at open")),
            "{:?}",
            verification.problems
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        store.checkpoint().expect("checkpoint");
        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
    }

    #[test]
    fn readers_stop_at_the_snapshot_and_the_next_append_cuts_off_a_partial_line() {
        let dir = temp_dir("partial");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        let before = store.ledger_head().expect("head");
        // Half a line past the chain: what a reader racing an append in
        // flight would meet at EOF, or what a failed append leaves when its
        // own rollback fails too.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(LOG_NAME))
            .expect("append");
        file.write_all(b"{\"seq\":2,\"kind\":\"anch")
            .expect("partial");
        drop(file);

        let verification = store.verify_ledger().expect("verify");
        assert!(verification.intact, "{verification:?}");
        assert_eq!(verification.head.as_deref(), Some(before.head.as_str()));
        let pack = store.evidence(None, None, None).expect("pack");
        let ledger = &pack["ledger"];
        assert_eq!(ledger["entries_unreadable"], json!(0));
        assert_eq!(ledger["entries"].as_array().expect("entries").len(), 2);
        assert_eq!(ledger["verification"]["head"], ledger["head"]["head"]);
        let (head, page) = store.ledger_page(None, MAX_PAGE).expect("page");
        assert_eq!(head.seq, before.seq);
        assert_eq!(page.len(), 2);

        // The next append cuts the partial line off before writing its own.
        store.checkpoint().expect("checkpoint");
        let text = fs::read_to_string(dir.join(LOG_NAME)).expect("read");
        assert!(!text.contains("anch"), "{text}");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert!(report.problems().is_empty(), "{:?}", report.problems());
    }

    #[test]
    fn verification_stays_intact_while_anchors_append_concurrently() {
        let dir = temp_dir("concurrent");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        let genesis: Entry = serde_json::from_str(&lines(&dir)[0]).expect("genesis");
        std::thread::scope(|scope| {
            let appender = scope.spawn(|| {
                // Wide lines, so a read to EOF would routinely meet one
                // half-written.
                for _ in 0..200 {
                    store
                        .anchor_ledger(anchor_request(0, &genesis.head, Some("x".repeat(4096))))
                        .expect("anchor");
                }
            });
            for _ in 0..20 {
                let verification = store.verify_ledger().expect("verify");
                assert!(verification.intact, "{:?}", verification.problems);
                let (head, page) = store.ledger_page(None, MAX_PAGE).expect("page");
                assert!(page
                    .iter()
                    .all(|entry| entry["seq"].as_u64().is_some_and(|seq| seq <= head.seq)));
            }
            appender.join().expect("appender");
        });
        assert!(store.verify_ledger().expect("verify").intact);
    }

    #[test]
    fn a_pin_copy_ends_on_its_checkpoint_whatever_was_appended_after() {
        let dir = temp_dir("pincopy");
        let ledger = Ledger::open(&dir).expect("open");
        ledger.record_checkpoint(5, digest(5)).expect("cp");
        // An erasure and an anchor landing after the pinned checkpoint, as
        // they can between a pin's checkpoint and its copy (neither takes
        // the locks a pin holds).
        ledger
            .record_erasure(1, &trace_subject("t1", ""), 5)
            .expect("erasure");
        let tip = ledger.head().expect("head");
        ledger
            .anchor(anchor_request(tip.seq, &tip.head, None))
            .expect("anchor");

        let target = dir.join("copy.jsonl");
        assert!(ledger.copy_through_checkpoint(5, &target).expect("copy"));
        let copied = fs::read_to_string(&target).expect("copied");
        assert!(copied.ends_with('\n'));
        assert_eq!(copied.lines().count(), 2);
        let last: Value =
            serde_json::from_str(copied.lines().last().expect("a line")).expect("entry");
        assert_eq!(last["kind"], "checkpoint");
        assert_eq!(last["generation"], json!(5));

        // A generation whose entry is not the latest checkpoint: no copy.
        let none = dir.join("none.jsonl");
        assert!(!ledger.copy_through_checkpoint(6, &none).expect("copy"));
        assert!(!none.exists());

        // The bound survives a reopen: it is recomputed from the scan.
        drop(ledger);
        let ledger = Ledger::open(&dir).expect("reopen");
        let again = dir.join("again.jsonl");
        assert!(ledger.copy_through_checkpoint(5, &again).expect("copy"));
        assert_eq!(
            fs::read(&again).expect("again"),
            fs::read(&target).expect("copy")
        );
    }

    /// Appends `count` valid anchor entries (each over the genesis head) to
    /// the ledger of a store that is not open, in one write: a long chain
    /// without one fsync per entry.
    fn grow_ledger(dir: &Path, count: u64) {
        let all = lines(dir);
        let genesis: Value = serde_json::from_str(&all[0]).expect("genesis");
        let last: Value = serde_json::from_str(all.last().expect("an entry")).expect("entry");
        let mut seq = last["seq"].as_u64().expect("seq");
        let mut prev = last["head"].as_str().expect("head").to_owned();
        let at_ns = last["at_ns"].as_u64().expect("at_ns");
        let mut text = String::new();
        for n in 0..count {
            seq += 1;
            let mut entry = json!({
                "seq": seq, "kind": "anchor", "at_ns": at_ns + n + 1, "prev": prev,
                "target_seq": 0, "target_head": genesis["head"],
                "authority": "notary.example", "anchored_at_ns": 42,
                "signature": "c2ln", "public_key": "a2V5",
            });
            let head = compute_head(&prev, &entry);
            entry["head"] = json!(head);
            text.push_str(&entry.to_string());
            text.push('\n');
            prev = head;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(LOG_NAME))
            .expect("ledger");
        file.write_all(text.as_bytes()).expect("grow");
    }

    #[test]
    fn an_evidence_pack_extends_the_verified_chain_instead_of_rewalking_it() {
        // A tenant's pack is the bound-credential path to a verification;
        // each one re-walking and re-hashing the whole chain would make the
        // chain's length the price of every request. What a request pays
        // for is what is new since the last pack.
        let dir = temp_dir("incremental");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        grow_ledger(&dir, 3000);
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let recent = |store: &Store| store.ledger_head().expect("head").at_ns;

        let before = probe::parsed();
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        assert!(
            probe::parsed() - before >= 3000,
            "the first pack walks it all"
        );
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));

        let before = probe::parsed();
        let since = recent(&store);
        let pack = store
            .evidence(Some("acme"), Some(since), None)
            .expect("pack");
        let parsed = probe::parsed() - before;
        assert!(
            parsed <= 2,
            "nothing new: no chain line is re-walked, and only the window's lines are \
             read ({parsed} parsed)"
        );
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        assert_eq!(
            pack["ledger"]["entries"].as_array().expect("entries").len(),
            1
        );

        // One anchor appended: one line walked, and the window holds it.
        let tip = store.ledger_head().expect("head");
        store
            .anchor_ledger(anchor_request(tip.seq, &tip.head, None))
            .expect("anchor");
        let before = probe::parsed();
        let pack = store
            .evidence(Some("acme"), Some(recent(&store)), None)
            .expect("pack");
        assert!(probe::parsed() - before <= 2);
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        assert_eq!(
            pack["ledger"]["head"]["seq"],
            json!(tip.seq + 1),
            "the pack covers the new entry"
        );

        // The operator's verification never reuses: the whole chain.
        let before = probe::parsed();
        assert!(store.verify_ledger().expect("verify").intact);
        assert!(probe::parsed() - before >= 3000);
    }

    #[test]
    fn an_evidence_pack_stops_its_chain_walk_at_the_deadline_and_resumes_there() {
        // A budget that is spent before the request begins: every budgeted
        // step stops at its first check. The chain walk must be one of
        // them — and must keep its progress, or a chain too long for one
        // budget could never be verified at all.
        let dir = temp_dir("deadline");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        grow_ledger(&dir, 1000);
        let total = lines(&dir).len() as u64;
        let config = crate::Config {
            query_deadline: Some(Duration::ZERO),
            ..crate::Config::default()
        };
        let store = Store::open(&dir, config).expect("reopen");
        let mut cut_short = 0_u64;
        loop {
            let before = probe::parsed();
            let outcome = store.evidence(Some("acme"), None, None);
            let parsed = probe::parsed() - before;
            match outcome {
                Err(Error::DeadlineExceeded(detail)) if detail.contains("ledger chain") => {
                    cut_short += 1;
                    assert!(
                        parsed <= WALK_CHECK_INTERVAL,
                        "one interval, then the check stops it: {parsed} parsed"
                    );
                    assert!(
                        detail.contains("resumes"),
                        "the refusal says the work is kept: {detail}"
                    );
                }
                other => {
                    // The walk is done; the activity fold is the next step
                    // over budget.
                    assert!(
                        matches!(other, Err(Error::DeadlineExceeded(_))),
                        "{other:?}"
                    );
                    break;
                }
            }
            assert!(
                cut_short <= total / WALK_CHECK_INTERVAL + 1,
                "each request continues where the last stopped"
            );
        }
        assert!(
            cut_short >= total / WALK_CHECK_INTERVAL,
            "the walk was cut short at every check: {cut_short} of {total} lines"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_walk_reads_a_final_line_again_once_its_newline_lands() {
        // The one snapshot that does not end on a newline: a final entry the
        // open could not terminate. A pack takes that line in on a copy and
        // keeps its walk short of it, because the next append writes the
        // missing newline first — a walk kept past the line would read that
        // newline as a blank line of its own.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unterminated");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        let path = dir.join(LOG_NAME);
        let text = fs::read_to_string(&path).expect("ledger");
        fs::write(&path, text.trim_end_matches('\n')).expect("unterminate");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("chmod");
        if OpenOptions::new().append(true).open(&path).is_ok() {
            eprintln!(
                "SKIPPED: this user can write a read-only file (root?); \
                 the unterminated final line was not exercised"
            );
            return;
        }
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let damage_free = |pack: &Value| {
            let problems = pack["ledger"]["verification"]["problems"].to_string();
            !problems.contains("blank") && !problems.contains("not a JSON object")
        };
        let pack = store.evidence(None, None, None).expect("pack");
        assert!(damage_free(&pack), "{pack}");
        assert_eq!(
            pack["ledger"]["entries"].as_array().expect("entries").len(),
            2
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        store.checkpoint().expect("checkpoint");
        let pack = store.evidence(None, None, None).expect("pack");
        assert!(damage_free(&pack), "{pack}");
        assert_eq!(
            pack["ledger"]["verification"]["intact"],
            json!(true),
            "{pack}"
        );
        assert_eq!(
            pack["ledger"]["entries"].as_array().expect("entries").len(),
            3
        );
    }

    #[test]
    fn an_evidence_chain_walk_holds_no_maintenance_lock() {
        // The walk reads an immutable prefix; nothing about it needs
        // checkpoints, compaction or expiry held off — so a long chain must
        // not hold them off either.
        let dir = temp_dir("unlocked-walk");
        let store = std::sync::Arc::new(Store::open(&dir, crate::Config::default()).expect("open"));
        let seen = std::sync::Arc::new(Mutex::new(None));
        let (probe_store, sink) = (std::sync::Arc::clone(&store), std::sync::Arc::clone(&seen));
        probe::on_walk(Box::new(move || {
            let free = probe_store.maintenance.try_lock().is_ok();
            *sink.lock().expect("sink") = Some(free);
        }));
        store.evidence(Some("acme"), None, None).expect("pack");
        assert_eq!(
            *seen.lock().expect("seen"),
            Some(true),
            "the walk ran, with the maintenance lock free"
        );
    }

    #[test]
    fn an_evidence_pack_states_its_caps_and_counts_what_it_cannot_place() {
        let dir = temp_dir("caps");
        {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            for trace in ["t1", "t2"] {
                store.ingest(test_span(trace, "")).expect("ingest");
            }
            for trace in ["t1", "t2"] {
                store.erase(trace_subject(trace, "")).expect("erase");
            }
        }
        // A damaged line inside the chain, right after the genesis: a JSON
        // object with no seq. It must neither end the scan nor vanish.
        let mut all = lines(&dir);
        all.insert(1, "{\"kind\":\"checkpoint\"}".to_owned());
        fs::write(dir.join(LOG_NAME), all.join("\n") + "\n").expect("write");

        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let pack = store.evidence(None, None, None).expect("pack");
        let ledger = &pack["ledger"];
        // Six readable entries (the genesis, the first open's checkpoint,
        // and a checkpoint and an erasure per erasure), capped.
        assert_eq!(
            ledger["entries"].as_array().expect("entries").len(),
            EVIDENCE_MAX_ENTRIES
        );
        assert_eq!(ledger["entries_truncated"], json!(true));
        assert_eq!(ledger["entries_unreadable"], json!(1));
        assert_eq!(ledger["verification"]["intact"], json!(false));

        let erasures = pack["erasures"].as_array().expect("erasures");
        assert_eq!(erasures.len(), 2);
        assert!(erasures[0]["conclusive"].is_boolean());
        assert_eq!(erasures[0]["result"], "erased");
        assert_eq!(
            erasures[1]["conclusive"],
            Value::Null,
            "past the receipt cap"
        );
        assert_eq!(erasures[1]["result"], Value::Null);
    }

    /// Records each of `subjects` as erased and then settled — published by
    /// `generation`, settled at its own time — straight into the tombstone
    /// log of a store that is not open, in one write: many settled erasures
    /// without a purge and a checkpoint apiece. The next open ledgers them.
    fn settle_erasures(dir: &Path, generation: u64, subjects: &[(erasure::Subject, u64)]) {
        let path = dir.join(erasure::LOG_NAME);
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        let mut id = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|record| record["id"].as_u64())
            .max()
            .unwrap_or(0);
        for (subject, settled_at) in subjects {
            id += 1;
            let mut erase = serde_json::to_value(erasure::EraseRecord::new(
                id,
                settled_at - 1,
                subject.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .expect("erase");
            erase["op"] = json!("erase");
            let mut settle = serde_json::to_value(erasure::SettleRecord {
                schema: erasure::SettleRecord::schema_now(),
                id,
                settled_unix_ns: *settled_at,
                generation,
                spans_removed: 0,
                spans_redacted: 0,
                annotations_removed: 0,
                payloads_removed: Vec::new(),
                payloads_retained: Vec::new(),
                eval_records_removed: 0,
            })
            .expect("settle");
            settle["op"] = json!("settle");
            for record in [erase, settle] {
                text.push_str(&record.to_string());
                text.push('\n');
            }
        }
        fs::write(&path, text).expect("tombstones");
    }

    /// `count` trace subjects of `tenant`, settled 1,000 ns apart from `from`.
    fn erased_traces(tenant: &str, count: u64, from: u64) -> Vec<(erasure::Subject, u64)> {
        (0..count)
            .map(|n| {
                (
                    trace_subject(&format!("{tenant}-t{n}"), tenant),
                    from + n * 1_000,
                )
            })
            .collect()
    }

    /// A query budget no request in these tests reaches, so budget checks
    /// run — and `probe::allow` decides them.
    fn budgeted() -> crate::Config {
        crate::Config {
            query_deadline: Some(Duration::from_secs(3600)),
            ..crate::Config::default()
        }
    }

    fn live_of(dir: &Path) -> u64 {
        generation::read_current(dir)
            .expect("CURRENT")
            .expect("a published generation")
    }

    #[test]
    fn an_unfinished_evidence_walk_finishes_across_the_reuse_window() {
        // A chain longer than one budget, and every retry arriving after the
        // reuse window has passed since the walk began. What the unfinished
        // walk established is about a fixed prefix of an append-only file,
        // and none of it goes stale while the walk is unfinished: retrying
        // must finish it, and the pack must then say how old it is.
        let dir = temp_dir("expiry");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        grow_ledger(&dir, 1000);
        let total = lines(&dir).len() as u64;
        // A budget no request reaches: the probe's allowance is what cuts
        // each walk short.
        let store = Store::open(&dir, budgeted()).expect("reopen");
        let checks = 2;
        let bound = total / (WALK_CHECK_INTERVAL * checks) + 3;
        let crossing = EVIDENCE_VERIFY_REUSE + Duration::from_secs(1);
        let mut attempts = 0_u64;
        let pack = loop {
            attempts += 1;
            assert!(
                attempts <= bound,
                "retrying never finished the walk: {attempts} attempts on {total} lines"
            );
            probe::allow(Some(checks));
            let outcome = store.evidence(Some("acme"), None, None);
            probe::allow(None);
            match outcome {
                Ok(pack) => break pack,
                Err(Error::DeadlineExceeded(detail)) => {
                    assert!(detail.contains("ledger chain"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            store.ledger.advance_clock(crossing);
        };
        assert!(attempts > 2, "the walk took several attempts: {attempts}");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        let generated = pack["generated_at_ns"].as_u64().expect("generated_at_ns");
        let walked_from = verification["chain_verified_at_ns"]
            .as_u64()
            .expect("the pack says when the walk it rests on began");
        let crossed = crossing.as_nanos() as u64 * (attempts - 1);
        assert!(
            generated.saturating_sub(walked_from) >= crossed,
            "the walk began {} ns before the pack, across {} crossings of the window",
            generated.saturating_sub(walked_from),
            attempts - 1
        );
    }

    #[test]
    fn a_warm_evidence_pack_does_no_work_per_settled_erasure() {
        // Every settled erasure is cross-checked against the chain, and a
        // tenant can ask for a pack as often as it likes: the cross-check
        // must cost what changed since the last pack, not every erasure the
        // store ever settled.
        let dir = temp_dir("many-erasures");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        settle_erasures(&dir, live_of(&dir), &erased_traces("acme", 120, 1_000));
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let pack = store.evidence(Some("acme"), None, None).expect("cold pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");

        let since = store.ledger_head().expect("head").at_ns;
        let (digested, examined) = (probe::digested(), probe::examined());
        let pack = store
            .evidence(Some("acme"), Some(since), None)
            .expect("warm pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        assert_eq!(
            probe::digested() - digested,
            0,
            "a warm pack digests no settled erasure again"
        );
        let examined = probe::examined() - examined;
        assert!(
            examined <= 64,
            "nor looks at each of them: {examined} examined"
        );

        // One erasure more: the next pack takes in that one, and only it.
        store.ingest(test_span("t-new", "acme")).expect("ingest");
        store.erase(trace_subject("t-new", "acme")).expect("erase");
        let digested = probe::digested();
        let pack = store
            .evidence(Some("acme"), Some(since), None)
            .expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(probe::digested() - digested, 1);
    }

    #[test]
    fn an_evidence_pack_indexes_settled_erasures_a_budget_at_a_time() {
        // Taking the settled erasures in is budgeted like the chain walk, and
        // keeps its progress: however many requests it takes, each settled
        // erasure is digested once.
        let dir = temp_dir("settle-budget");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        let count = 150;
        settle_erasures(&dir, live_of(&dir), &erased_traces("acme", count, 1_000));
        let store = Store::open(&dir, budgeted()).expect("reopen");
        let before = probe::digested();
        let (mut refused, mut indexing) = (0_u64, 0_u64);
        loop {
            probe::allow(Some(0));
            let outcome = store.evidence(Some("acme"), None, None);
            probe::allow(None);
            match outcome {
                Ok(pack) => {
                    let verification = &pack["ledger"]["verification"];
                    assert_eq!(verification["intact"], json!(true), "{verification}");
                    break;
                }
                Err(Error::DeadlineExceeded(detail)) => {
                    refused += 1;
                    if detail.contains("settled erasure") {
                        indexing += 1;
                        assert!(detail.contains("resumes"), "{detail}");
                    }
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(refused < 100, "retrying finishes");
        }
        assert!(
            indexing >= 2,
            "{count} settled erasures took several budgets to take in: {indexing}"
        );
        assert_eq!(
            probe::digested() - before,
            count,
            "each settled erasure digested once, however many requests it took"
        );
    }

    #[test]
    fn an_evidence_window_is_found_without_scanning_the_chain() {
        // A window of a long chain — empty, or holding a few entries — is a
        // lookup in the walk's index, not a pass over it.
        let dir = temp_dir("window-index");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        grow_ledger(&dir, 3000);
        let at: Vec<u64> = lines(&dir)
            .iter()
            .map(|line| {
                serde_json::from_str::<Value>(line).expect("entry")["at_ns"]
                    .as_u64()
                    .expect("at_ns")
            })
            .collect();
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        store.evidence(Some("acme"), None, None).expect("cold pack");
        for (since, until) in [(1, 2), (u64::MAX - 1, u64::MAX)] {
            let before = probe::examined();
            let pack = store
                .evidence(Some("acme"), Some(since), Some(until))
                .expect("pack");
            let examined = probe::examined() - before;
            assert!(
                examined <= 64,
                "an empty window is a lookup, not a scan: {examined} examined"
            );
            assert!(pack["ledger"]["entries"]
                .as_array()
                .expect("entries")
                .is_empty());
        }
        let pack = store
            .evidence(Some("acme"), Some(at[100]), Some(at[102]))
            .expect("pack");
        let seqs: Vec<u64> = pack["ledger"]["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|entry| entry["seq"].as_u64().expect("seq"))
            .collect();
        assert_eq!(seqs, [100, 101, 102]);
    }

    #[test]
    fn the_erasures_a_pack_lists_are_found_by_index_and_capped() {
        let dir = temp_dir("erasure-listing");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        let mut subjects = erased_traces("acme", 10, 10_000);
        subjects.extend(erased_traces("bigco", 5, 10_500));
        settle_erasures(&dir, live_of(&dir), &subjects);
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let published = |pack: &Value| -> Vec<u64> {
            pack["erasures"]
                .as_array()
                .expect("erasures")
                .iter()
                .map(|row| row["published_at_ns"].as_u64().expect("published_at_ns"))
                .collect()
        };

        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        assert_eq!(
            published(&pack),
            (0..EVIDENCE_MAX_ERASURES as u64)
                .map(|n| 10_000 + n * 1_000)
                .collect::<Vec<_>>(),
            "acme's, oldest first, up to the cap"
        );
        assert_eq!(pack["erasures_truncated"], json!(true));
        let pack = store.evidence(None, None, None).expect("pack");
        assert_eq!(published(&pack).len(), EVIDENCE_MAX_ERASURES);
        assert_eq!(pack["erasures_truncated"], json!(true));

        let pack = store
            .evidence(Some("acme"), Some(12_000), Some(13_000))
            .expect("pack");
        assert_eq!(published(&pack), [12_000, 13_000]);
        assert_eq!(pack["erasures_truncated"], json!(false));

        let before = probe::examined();
        let pack = store
            .evidence(Some("acme"), Some(1_000_000), None)
            .expect("pack");
        assert!(published(&pack).is_empty());
        let examined = probe::examined() - before;
        assert!(
            examined <= 64,
            "a window holding none of them is a lookup: {examined} examined"
        );
    }

    #[test]
    fn a_cold_file_check_runs_off_the_maintenance_lock_and_resumes_where_it_stopped() {
        // Re-digesting the store is O(store bytes). A pack must not do it
        // while holding checkpoints, compaction and erasures off, nor in one
        // unbudgeted piece: it digests a budget at a time, and a file left
        // part-way through is continued, not started again.
        let dir = temp_dir("file-walk");
        let store = std::sync::Arc::new(Store::open(&dir, budgeted()).expect("open"));
        for n in 0..400_u32 {
            // Hex of distinct digests: about as incompressible as text gets.
            let noise: String = (0..30_u32)
                .map(|k| payload::sha256_hex(&[n.to_le_bytes(), k.to_le_bytes()].concat()))
                .collect();
            let span: crate::Span = serde_json::from_value(json!({
                "trace_id": format!("t{n}"), "span_id": "s1", "name": "chat", "service": "svc",
                "start_time_ns": 1_000u64 + u64::from(n), "end_time_ns": 2_000u64 + u64::from(n),
                "attributes": {"noise": noise},
            }))
            .expect("span");
            store.ingest(span).expect("ingest");
        }
        store.flush().expect("flush");
        let live = store.checkpoint().expect("checkpoint");
        let manifest =
            generation::load_manifest(&generation::manifest_path(&dir, live)).expect("manifest");
        let manifested: u64 = manifest.files.iter().map(|file| file.bytes).sum();
        assert!(manifested > 4 * 64 * 1024, "{manifested} manifested bytes");

        let seen = std::sync::Arc::new(Mutex::new(None));
        let (probe_store, sink) = (std::sync::Arc::clone(&store), std::sync::Arc::clone(&seen));
        probe::on_hash(Box::new(move || {
            let free = probe_store.maintenance.try_lock().is_ok();
            *sink.lock().expect("sink") = Some(free);
        }));
        let before = probe::hashed_bytes();
        let mut cut_short = 0_u64;
        loop {
            probe::allow(Some(0));
            let outcome = store.evidence(Some("acme"), None, None);
            probe::allow(None);
            match outcome {
                Ok(pack) => {
                    let verification = &pack["ledger"]["verification"];
                    assert_eq!(verification["intact"], json!(true), "{verification}");
                    break;
                }
                Err(Error::DeadlineExceeded(detail)) if detail.contains("files") => {
                    cut_short += 1;
                    assert!(detail.contains("resumes"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(cut_short < 1000, "retrying finishes");
        }
        assert!(
            cut_short >= 2,
            "the check was cut short and resumed: {cut_short}"
        );
        assert_eq!(
            *seen.lock().expect("seen"),
            Some(true),
            "files were digested with the maintenance lock free"
        );
        assert_eq!(
            probe::hashed_bytes() - before,
            manifested,
            "every manifested byte digested exactly once: nothing started over"
        );
    }

    #[test]
    fn a_time_index_finds_late_arrivals_in_arrival_order() {
        // Keys arrive in order; times mostly do. The ones that do not —
        // a clock stepping back — are found all the same, in arrival order.
        let mut index = TimeIndex::default();
        for (at_ns, key) in [(10, 0), (20, 1), (15, 2), (30, 3), (5, 4), (25, 5), (40, 6)] {
            index.push(at_ns, key);
        }
        let window = |since, until, cap, keep: fn(u64) -> bool| {
            index.window(since, until, cap, keep, None).expect("window")
        };
        assert_eq!(
            window(0, 100, 10, |_| true),
            (vec![0, 1, 2, 3, 4, 5, 6], false)
        );
        assert_eq!(window(12, 26, 10, |_| true), (vec![1, 2, 5], false));
        assert_eq!(window(0, 100, 3, |_| true), (vec![0, 1, 2], true));
        assert_eq!(window(0, 100, 3, |key| key != 1), (vec![0, 2, 3], true));
        assert_eq!(window(41, 50, 10, |_| true), (Vec::new(), false));
        assert_eq!(window(5, 5, 10, |_| true), (vec![4], false));
    }

    fn settlement(id: u64, trace: &str, generation: u64) -> erasure::Settlement {
        erasure::Settlement {
            id,
            subject: trace_subject(trace, "acme"),
            generation,
            settled_unix_ns: id,
        }
    }

    fn ledgered(seq: u64, id: u64, trace: &str, generation: u64) -> LedgeredErasure {
        LedgeredErasure {
            seq,
            erasure_id: id,
            subject_digest: hex_key(&subject_digest(&trace_subject(trace, "acme"))),
            generation: Some(generation),
            subject_kind: "trace".to_owned(),
            tenant: Some("acme".to_owned()),
        }
    }

    /// What a cross-check reports: the undone entries' seqs, in chain order,
    /// and the unledgered settles' `(id, generation)` — after checking that
    /// its ordered and per-tenant indexes hold exactly what its findings do.
    fn findings(cross: &CrossCheck, walk: &ChainWalk) -> (Vec<u64>, Vec<(u64, u64)>) {
        let by_subject: BTreeSet<usize> = cross
            .undone
            .values()
            .flatten()
            .map(|&(_, index)| index)
            .collect();
        assert_eq!(by_subject, cross.undone_order);
        let by_tenant: BTreeSet<usize> = cross.undone_scoped.values().flatten().copied().collect();
        assert_eq!(by_tenant, cross.undone_order);
        let by_tenant: BTreeSet<Unledgered> = cross
            .unledgered_scoped
            .values()
            .flatten()
            .copied()
            .collect();
        assert_eq!(
            by_tenant,
            cross.unledgered.keys().copied().collect::<BTreeSet<_>>()
        );
        (
            cross
                .undone_order
                .iter()
                .map(|&index| walk.erasures[index].seq)
                .collect(),
            cross
                .unledgered
                .keys()
                .map(|(id, generation, _)| (*id, *generation))
                .collect(),
        )
    }

    #[test]
    fn a_cross_check_taken_in_piecemeal_agrees_with_one_taken_whole() {
        // Kept up to date as settles and entries arrive — and started over
        // when an erasure settles a second time — the cross-check must say
        // exactly what one built from everything at once says, at every
        // step.
        let mut walk = ChainWalk::new();
        let mut settles = SettleIndex::default();
        let mut cross = CrossCheck::default();
        let mut agree = |walk: &ChainWalk, settles: &SettleIndex, expected| {
            assert_eq!(cross.sync(walk, settles, None), Walked::Reached);
            let mut whole = CrossCheck::default();
            assert_eq!(whole.sync(walk, settles, None), Walked::Reached);
            assert_eq!(findings(&cross, walk), findings(&whole, walk));
            assert_eq!(findings(&cross, walk), expected);
        };
        // Erasure 1 settled and ledgered: nothing to say.
        settles.push(&settlement(1, "t1", 5));
        walk.erasures.push(ledgered(3, 1, "t1", 5));
        agree(&walk, &settles, (vec![], vec![]));
        // Erasure 2 ledgered, its settle gone (a restore undid it); erasure
        // 3 settled, its entry not yet in.
        walk.erasures.push(ledgered(4, 2, "t2", 7));
        settles.push(&settlement(3, "t3", 8));
        agree(&walk, &settles, (vec![4], vec![(3, 8)]));
        // t2 erased again, later: that vouches for the undone entry; its own
        // entry is still to come.
        settles.push(&settlement(4, "t2", 9));
        agree(&walk, &settles, (vec![], vec![(3, 8), (4, 9)]));
        walk.erasures.push(ledgered(5, 3, "t3", 8));
        walk.erasures.push(ledgered(6, 4, "t2", 9));
        agree(&walk, &settles, (vec![], vec![]));
        // Erasure 1 settled a second time, later (two completions racing):
        // the later settle is the one that counts, and lacks an entry.
        settles.push(&settlement(1, "t1", 6));
        agree(&walk, &settles, (vec![], vec![(1, 6)]));
        // Erasure 3 settled a second time with an EARLIER generation: t3's
        // newest settled generation falls below its entry's.
        settles.push(&settlement(3, "t3", 2));
        agree(&walk, &settles, (vec![5], vec![(1, 6), (3, 2)]));
    }

    #[test]
    fn a_pack_and_the_operators_verification_agree_after_an_erasure_settles_twice() {
        // The store-level face of the case above: the pack's kept
        // cross-check, started over, says what a fresh verification says.
        let dir = temp_dir("settled-twice");
        let store = Store::open(&dir, crate::Config::default()).expect("open");
        store.ingest(test_span("t1", "acme")).expect("ingest");
        let erased = store.erase(trace_subject("t1", "acme")).expect("erase");
        let pack = store.evidence(None, None, None).expect("pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        let mut again = erased.settle.clone().expect("settled");
        again.generation += 100;
        store.erasures.record_settle(again).expect("settle again");

        let pack = store.evidence(None, None, None).expect("pack");
        let operator = store.verify_ledger().expect("verify");
        assert!(!operator.intact);
        assert_eq!(
            pack["ledger"]["verification"]["problems"],
            json!(operator.problems),
        );
        assert!(
            format!("{:?}", operator.problems).contains("is not in the ledger"),
            "{:?}",
            operator.problems
        );
    }

    #[test]
    fn a_stale_verification_is_served_with_its_age_while_packs_redo_it() {
        // Past the reuse window a completed walk is still answered from —
        // saying how old it is — while packs redo it from byte 0 a slice at
        // a time; the redo, once complete, is what packs answer from. An
        // edit under the running server shows then.
        let dir = temp_dir("stale-served");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        grow_ledger(&dir, 1000);
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let first = store.evidence(None, None, None).expect("pack");
        let walked_at = first["ledger"]["verification"]["chain_verified_at_ns"].clone();
        assert!(walked_at.is_u64());

        // An entry the walk already passed, edited in place.
        let path = dir.join(LOG_NAME);
        let text = fs::read_to_string(&path).expect("ledger");
        let edited = text.replacen("notary.example", "notary.exampl3", 1);
        assert_eq!(edited.len(), text.len());
        fs::write(&path, edited).expect("edit");
        store
            .ledger
            .advance_clock(EVIDENCE_VERIFY_REUSE + Duration::from_secs(1));

        let mut stale = 0_u64;
        let redone = loop {
            probe::allow(Some(2));
            let pack = store.evidence(None, None, None);
            probe::allow(None);
            let pack = pack.expect("a stale walk is answered from, not refused");
            let verification = pack["ledger"]["verification"].clone();
            if verification["chain_verified_at_ns"] != walked_at {
                break verification;
            }
            assert_eq!(verification["intact"], json!(true), "{verification}");
            let generated = pack["generated_at_ns"].as_u64().expect("generated_at_ns");
            let age = generated - walked_at.as_u64().expect("walked at");
            assert!(
                age > EVIDENCE_VERIFY_REUSE.as_nanos() as u64,
                "the pack states the walk's age: {age} ns"
            );
            stale += 1;
            assert!(stale < 100, "the redo finishes");
        };
        assert!(stale >= 2, "the redo took several packs: {stale}");
        assert_eq!(redone["intact"], json!(false), "{redone}");
        assert!(
            redone["problems"]
                .to_string()
                .contains("head does not match"),
            "{redone}"
        );
    }

    #[test]
    fn packs_redigest_files_past_the_window_and_only_what_a_checkpoint_changed() {
        let dir = temp_dir("file-refresh");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        settle_erasures(&dir, live_of(&dir), &erased_traces("acme", 3, 1_000));
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let manifest_of = |generation: u64| {
            generation::load_manifest(&generation::manifest_path(&dir, generation))
                .expect("manifest")
        };
        for trace in ["a1", "a2"] {
            store.ingest(test_span(trace, "acme")).expect("ingest");
        }
        store.flush().expect("flush");
        let first = manifest_of(store.checkpoint().expect("checkpoint"));
        let before = probe::hashed_bytes();
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        assert_eq!(
            probe::hashed_bytes() - before,
            first.files.iter().map(|file| file.bytes).sum::<u64>()
        );

        // A new generation: only what it added or changed is digested.
        for trace in ["b1", "b2"] {
            store.ingest(test_span(trace, "acme")).expect("ingest");
        }
        store.flush().expect("flush");
        let second = manifest_of(store.checkpoint().expect("checkpoint"));
        let changed: u64 = second
            .files
            .iter()
            .filter(|file| {
                !first.files.iter().any(|old| {
                    old.path == file.path
                        && old.bytes == file.bytes
                        && old.sha256 == file.sha256
                        && old.modified_unix_ns == file.modified_unix_ns
                })
            })
            .map(|file| file.bytes)
            .sum();
        assert!(changed > 0);
        let before = probe::hashed_bytes();
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        assert_eq!(probe::hashed_bytes() - before, changed);

        // A manifested file edited under the running server: a fresh digest
        // stands until it is past the window; then the pack answers from it,
        // saying how old it is, and redoes it — and the next pack has it.
        let tombstones = dir.join(erasure::LOG_NAME);
        let text = fs::read_to_string(&tombstones).expect("tombstones");
        let edited = text.replacen("\"settled_unix_ns\":1000", "\"settled_unix_ns\":1001", 1);
        assert_ne!(edited, text);
        fs::write(&tombstones, edited).expect("edit");
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        assert_eq!(pack["ledger"]["verification"]["intact"], json!(true));
        store
            .ledger
            .advance_clock(EVIDENCE_VERIFY_REUSE + Duration::from_secs(1));
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        let age = pack["generated_at_ns"].as_u64().expect("generated_at_ns")
            - verification["generation"]["verified_at_ns"]
                .as_u64()
                .expect("verified_at_ns");
        assert!(age > EVIDENCE_VERIFY_REUSE.as_nanos() as u64, "{age}");
        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["generation"]["files_verified"], json!(false));
        assert!(
            verification["problems"]
                .to_string()
                .contains(&format!("{}: digest mismatch", erasure::LOG_NAME)),
            "{verification}"
        );
    }

    #[test]
    fn a_new_generations_manifest_is_taken_in_off_the_maintenance_lock_a_budget_at_a_time() {
        // A checkpoint publishes a new manifest, and the next pack must take
        // it in before it can answer: read it, digest it, decode an entry per
        // manifested file. That is O(files), so it must neither hold the
        // maintenance lock while it runs (opening the manifest is all that
        // needs it) nor run in one unbudgeted piece: a budget at a time, and
        // each entry taken in once however many requests that takes.
        let dir = temp_dir("manifest-budget");
        let store = std::sync::Arc::new(Store::open(&dir, budgeted()).expect("open"));
        let count = 5 * WALK_CHECK_INTERVAL;
        write_payloads(&dir, count);
        let live = store.checkpoint().expect("checkpoint");
        let manifested = manifested_files(&dir, live);
        assert!(manifested >= count, "{manifested} manifested files");

        let seen = std::sync::Arc::new(Mutex::new(None));
        let (probe_store, sink) = (std::sync::Arc::clone(&store), std::sync::Arc::clone(&seen));
        probe::on_manifest(Box::new(move || {
            let free = probe_store.maintenance.try_lock().is_ok();
            *sink.lock().expect("sink") = Some(free);
        }));
        let before = probe::manifest_entries();
        let mut refused = 0_u64;
        loop {
            let taken = probe::manifest_entries();
            probe::allow(Some(0));
            let outcome = store.evidence(Some("acme"), None, None);
            probe::allow(None);
            let taken = probe::manifest_entries() - taken;
            assert!(
                taken < manifested,
                "one budget took in {taken} of {manifested} manifest entries"
            );
            match outcome {
                Ok(pack) => {
                    let verification = &pack["ledger"]["verification"];
                    assert_eq!(verification["intact"], json!(true), "{verification}");
                    break;
                }
                Err(Error::DeadlineExceeded(detail)) => {
                    refused += 1;
                    assert!(detail.contains("resumes"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(refused < 1000, "retrying finishes");
        }
        assert_eq!(
            probe::manifest_entries() - before,
            manifested,
            "each manifest entry taken in once, however many requests it took"
        );
        assert_eq!(
            *seen.lock().expect("seen"),
            Some(true),
            "the manifest was read with the maintenance lock free"
        );
    }

    /// Writes `count` payload files into `dir`'s payload tree, for the next
    /// checkpoint to manifest; returns their paths.
    fn write_payloads(dir: &Path, count: u64) -> Vec<PathBuf> {
        (0..count)
            .map(|n| {
                let path = dir
                    .join("payloads")
                    .join(format!("{:02x}", n % 256))
                    .join(format!("{n:08}.bin"));
                fs::create_dir_all(path.parent().expect("parent")).expect("dir");
                fs::write(&path, n.to_le_bytes()).expect("payload");
                path
            })
            .collect()
    }

    /// How many files generation `generation`'s manifest names.
    fn manifested_files(dir: &Path, generation: u64) -> u64 {
        generation::load_manifest(&generation::manifest_path(dir, generation))
            .expect("manifest")
            .files
            .len() as u64
    }

    /// One pack whose every budget check fails, so each of its steps makes
    /// its one check interval of progress and no more.
    fn starved_pack(store: &Store, tenant: Option<&str>) -> Result<Value> {
        probe::allow(Some(0));
        let outcome = store.evidence(tenant, None, None);
        probe::allow(None);
        outcome
    }

    #[test]
    fn a_manifest_longer_than_a_budget_is_taken_in_with_a_checkpoint_between_every_retry() {
        // A manifest that takes several budgets to take in, and a checkpoint
        // publishing between every retry: nothing changed, just another
        // generation live each time. A load restarted for each newer
        // generation never finished — every retry read a new manifest from
        // its first byte and was refused, and however many retries the
        // client made, no pack came of them. The load keeps the generation
        // and snapshot it began on until it is done, and the pack answers
        // for that consistent pair.
        let dir = temp_dir("manifest-churn");
        let store = Store::open(&dir, budgeted()).expect("open");
        write_payloads(&dir, 5 * WALK_CHECK_INTERVAL);
        let first = store.checkpoint().expect("checkpoint");
        let manifested = manifested_files(&dir, first);
        let head_then = store.ledger_head().expect("head");

        let before = probe::manifest_entries();
        let mut refused = 0_u64;
        let pack = loop {
            match starved_pack(&store, Some("acme")) {
                Ok(pack) => break pack,
                Err(Error::DeadlineExceeded(detail)) => {
                    refused += 1;
                    assert!(detail.contains("resumes"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(
                refused < 200,
                "retrying finishes, though every retry finds another generation live"
            );
            store.checkpoint().expect("checkpoint");
        };
        assert!(
            refused > 1,
            "the manifest took {refused} refusals, not several"
        );
        assert!(store.live_generation() > first + 1, "generations moved on");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(
            verification["generation"]["id"],
            json!(first),
            "the pack answers for the generation its load began on"
        );
        assert_eq!(verification["generation"]["matches_ledger"], json!(true));
        assert_eq!(
            pack["ledger"]["head"]["seq"],
            json!(head_then.seq),
            "and for the chain as it stood then"
        );
        assert_eq!(
            probe::manifest_entries() - before,
            manifested,
            "each entry of that manifest taken in once"
        );

        // Done, the next pack is about the generation live now.
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(
            verification["generation"]["id"],
            json!(store.live_generation())
        );
        assert_eq!(
            pack["ledger"]["head"]["seq"],
            json!(store.ledger_head().expect("head").seq)
        );
    }

    #[test]
    fn a_pinned_pack_past_the_reuse_window_answers_for_the_store_as_it_is() {
        // The pair a pack pinned to take its manifest in is answered for
        // however long that takes — but once the manifest is in and the pin
        // is older than the reuse window, a pack about a chain that old is
        // not worth finishing: the next round snapshots afresh, carrying
        // over every file digest by unchanged entry, and answers for the
        // generation live then.
        let dir = temp_dir("pin-age");
        let store = Store::open(&dir, budgeted()).expect("open");
        write_payloads(&dir, 5 * WALK_CHECK_INTERVAL);
        let first = store.checkpoint().expect("checkpoint");
        let taken_in = format!("files of generation {first} were digested");
        let mut refused = 0_u64;
        loop {
            match starved_pack(&store, None) {
                Err(Error::DeadlineExceeded(detail)) if detail.contains(&taken_in) => break,
                Err(Error::DeadlineExceeded(_)) => refused += 1,
                other => panic!("the manifest is in before the files: {other:?}"),
            }
            assert!(refused < 100, "the manifest is taken in");
            store.checkpoint().expect("checkpoint");
        }
        store.checkpoint().expect("checkpoint");
        store
            .ledger
            .advance_clock(EVIDENCE_VERIFY_REUSE + Duration::from_secs(1));
        let pack = loop {
            match starved_pack(&store, None) {
                Ok(pack) => break pack,
                Err(Error::DeadlineExceeded(_)) => refused += 1,
                Err(other) => panic!("{other:?}"),
            }
            assert!(refused < 1000, "retrying finishes");
        };
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(
            verification["generation"]["id"],
            json!(store.live_generation()),
            "the stale pin was let go"
        );
        assert_eq!(
            pack["ledger"]["head"]["seq"],
            json!(store.ledger_head().expect("head").seq)
        );
    }

    #[test]
    fn many_file_problems_are_confirmed_a_bounded_batch_per_maintenance_hold() {
        // Many manifested files removed by hand. Budget-limited packs find
        // them missing a few at a time, each problem provisional until it is
        // confirmed holding the maintenance lock. Once the last file was
        // checked, one request confirmed every problem the earlier ones had
        // found, in one hold, with no budget check — however many there
        // were. Confirmation is a batch per hold, with the budget checked
        // between holds and the rest kept for the next request.
        let dir = temp_dir("confirm-batches");
        let store = Store::open(&dir, budgeted()).expect("open");
        let paths = write_payloads(&dir, 5 * WALK_CHECK_INTERVAL);
        store.checkpoint().expect("checkpoint");
        for path in &paths {
            fs::remove_file(path).expect("remove");
        }
        let (mut most, mut refused) = (0_u64, 0_u64);
        let pack = loop {
            let confirmed = probe::confirmations();
            let outcome = starved_pack(&store, None);
            most = most.max(probe::confirmations() - confirmed);
            match outcome {
                Ok(pack) => break pack,
                Err(Error::DeadlineExceeded(detail)) => {
                    refused += 1;
                    assert!(detail.contains("resumes"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(refused < 1000, "retrying finishes");
        };
        assert!(
            most <= CONFIRM_BATCH as u64,
            "one request confirmed {most} problems, past a batch of {CONFIRM_BATCH}"
        );
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(false), "{verification}");
        assert_eq!(verification["generation"]["files_verified"], json!(false));
        assert!(
            verification["problems"]
                .to_string()
                .contains(&format!("({} in all)", paths.len())),
            "every removed file counted: {verification}"
        );
    }

    #[test]
    fn a_replaced_file_check_is_freed_a_budget_at_a_time_off_the_maintenance_lock() {
        // A file check holds a state per manifested file, and so does a
        // manifest part-way through being taken in. Replacing the check
        // with the next generation's freed the old one whole, in one
        // unbudgeted piece; and a load part-way through when the store
        // published again was dropped holding the maintenance lock, where
        // checkpoints, compaction and expiry wait for it.
        let dir = temp_dir("reclaim");
        let store = Store::open(&dir, budgeted()).expect("open");
        write_payloads(&dir, 5 * WALK_CHECK_INTERVAL);
        let first = store.checkpoint().expect("checkpoint");
        let manifested = manifested_files(&dir, first);
        store.evidence(None, None, None).expect("a first pack");

        // The next generation's manifest, a budget's share of it taken in;
        // then the store publishes again before the next pack.
        let second = store.checkpoint().expect("checkpoint");
        assert!(manifested_files(&dir, second) >= manifested);
        assert!(matches!(
            starved_pack(&store, None),
            Err(Error::DeadlineExceeded(_))
        ));
        store.checkpoint().expect("checkpoint");

        let (freed_before, _) = probe::freed_files();
        let (mut most, mut refused) = (0_u64, 0_u64);
        loop {
            let (freed, _) = probe::freed_files();
            let outcome = starved_pack(&store, None);
            most = most.max(probe::freed_files().0 - freed);
            match outcome {
                Ok(_) => break,
                Err(Error::DeadlineExceeded(_)) => refused += 1,
                Err(other) => panic!("{other:?}"),
            }
            assert!(refused < 1000, "retrying finishes");
        }
        // Whatever is left of the replaced check goes in later packs' time.
        for _ in 0..1000 {
            if probe::freed_files().0 - freed_before >= manifested {
                break;
            }
            let (freed, _) = probe::freed_files();
            let _ = starved_pack(&store, None);
            most = most.max(probe::freed_files().0 - freed);
        }
        assert_eq!(
            probe::freed_files().1,
            0,
            "a file check was freed holding the maintenance lock"
        );
        assert!(
            probe::freed_files().0 - freed_before >= manifested,
            "the replaced check is freed in the end"
        );
        assert!(
            most <= 2 * RECLAIM_CHUNK as u64,
            "one request freed {most} files' states of a {manifested}-file check"
        );
    }

    #[test]
    fn a_rollback_that_undoes_many_erasures_is_counted_not_itemized_by_packs() {
        // Restoring a pin from before many erasures leaves every one of their
        // entries undone. A pack must not collect, sort and format all of
        // them on every request — least of all for a tenant they do not
        // concern. It itemizes a few of its reader's own, counts the rest,
        // and says that it did.
        let dir = temp_dir("rollback-many");
        {
            let store = Store::open(&dir, crate::Config::default()).expect("open");
            store.ingest(test_span("kept", "cleanco")).expect("ingest");
            store.pin_generation("before").expect("pin");
        }
        let per_tenant = 3 * EVIDENCE_MAX_PROBLEMS as u64;
        let mut subjects = erased_traces("acme", per_tenant, 1_000);
        subjects.extend(erased_traces("bigco", per_tenant, 1_000_000));
        settle_erasures(&dir, live_of(&dir), &subjects);
        drop(Store::open(&dir, crate::Config::default()).expect("reopen: ledgers them"));
        let store = Store::restore(&dir, dir.join("pins/before"), crate::Config::default())
            .expect("restore");
        let total = 2 * per_tenant;
        let undone = |problems: &Value| {
            problems
                .as_array()
                .expect("problems")
                .iter()
                .filter(|problem| {
                    problem
                        .as_str()
                        .is_some_and(|p| p.contains("holds no settled"))
                })
                .count()
        };

        // The operator's verification names every one.
        let operator = store.verify_ledger().expect("verify");
        assert_eq!(
            undone(&json!(operator.problems)),
            total as usize,
            "{operator:?}"
        );

        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(false));
        assert_eq!(
            undone(&verification["problems"]),
            EVIDENCE_MAX_PROBLEMS,
            "{verification}"
        );
        assert_eq!(
            verification["problems_truncated"],
            json!(true),
            "{verification}"
        );
        assert!(
            verification["problems"]
                .to_string()
                .contains(&format!("({total} in all)")),
            "{verification}"
        );

        // acme's: a few of its own with their total, the rest one count.
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        let problems = verification["problems"].to_string();
        assert_eq!(
            undone(&verification["problems"]),
            EVIDENCE_MAX_PROBLEMS,
            "{verification}"
        );
        assert_eq!(
            verification["problems_truncated"],
            json!(true),
            "{verification}"
        );
        assert!(
            problems.contains(&format!("({per_tenant} in all)")),
            "{problems}"
        );
        assert!(
            problems.contains(&format!(
                "{per_tenant} problem(s) about data outside this tenant are withheld"
            )),
            "{problems}"
        );
        assert!(!problems.contains("bigco"), "{problems}");

        // A tenant they do not concern: nothing itemized, one count, and
        // nothing of its own left out.
        let pack = store.evidence(Some("cleanco"), None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(false));
        assert_eq!(undone(&verification["problems"]), 0, "{verification}");
        assert_eq!(
            verification["problems_truncated"],
            json!(false),
            "{verification}"
        );
        assert!(
            verification["problems"].to_string().contains(&format!(
                "{total} problem(s) about data outside this tenant are withheld"
            )),
            "{verification}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn settled_erasures_the_chain_lacks_are_counted_not_itemized_by_packs() {
        // The other direction: many settled erasures with no entry, because
        // the ledger cannot be appended to. Counted past the cap likewise.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unledgered-many");
        drop(Store::open(&dir, crate::Config::default()).expect("open"));
        let per_tenant = 3 * EVIDENCE_MAX_PROBLEMS as u64;
        let mut subjects = erased_traces("acme", per_tenant, 1_000);
        subjects.extend(erased_traces("bigco", per_tenant, 1_000_000));
        settle_erasures(&dir, live_of(&dir), &subjects);
        let path = dir.join(LOG_NAME);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("chmod");
        if OpenOptions::new().append(true).open(&path).is_ok() {
            eprintln!(
                "SKIPPED: this user can write a read-only file (root?); \
                 the unledgered erasures were not exercised"
            );
            return;
        }
        let store = Store::open(&dir, crate::Config::default()).expect("reopen");
        let total = 2 * per_tenant;
        let missing = |problems: &Value| {
            problems
                .as_array()
                .expect("problems")
                .iter()
                .filter(|problem| {
                    problem
                        .as_str()
                        .is_some_and(|p| p.contains("is not in the ledger"))
                })
                .count()
        };
        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(
            missing(&verification["problems"]),
            EVIDENCE_MAX_PROBLEMS,
            "{verification}"
        );
        assert_eq!(
            verification["problems_truncated"],
            json!(true),
            "{verification}"
        );
        assert!(
            verification["problems"]
                .to_string()
                .contains(&format!("({total} in all)")),
            "{verification}"
        );
        let pack = store.evidence(Some("acme"), None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        let problems = verification["problems"].to_string();
        assert_eq!(
            missing(&verification["problems"]),
            EVIDENCE_MAX_PROBLEMS,
            "{verification}"
        );
        assert!(
            problems.contains(&format!("({per_tenant} in all)")),
            "{problems}"
        );
        assert!(
            problems.contains(&format!(
                "{per_tenant} problem(s) about data outside this tenant are withheld"
            )),
            "{problems}"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    }

    #[test]
    fn healthy_maintenance_during_an_off_lock_file_check_is_not_reported_as_damage() {
        // A pack snapshots generation G under the maintenance lock and
        // digests its files after releasing it. Before it reaches the
        // segments, expiry unlinks one whose every span expired and renames a
        // rewritten survivor set over the other, and a checkpoint publishes
        // G+1 naming only the survivor. Nothing is damaged — G's files were
        // intact when G was live, and G+1's are — so the pack must not say
        // otherwise.
        let dir = temp_dir("maintenance-race");
        let store = std::sync::Arc::new(Store::open(&dir, crate::Config::default()).expect("open"));
        let span = |trace: &str, end: u64| -> crate::Span {
            serde_json::from_value(json!({
                "trace_id": trace, "span_id": "s1", "name": "chat", "service": "svc",
                "start_time_ns": end - 1, "end_time_ns": end,
            }))
            .expect("span")
        };
        store.ingest(span("old-1", 2_000)).expect("ingest");
        store.flush().expect("flush");
        store.ingest(span("old-2", 2_000)).expect("ingest");
        store
            .ingest(span("new-1", 4_000_000_000_000_000_000))
            .expect("ingest");
        store.flush().expect("flush");
        let before = store.checkpoint().expect("checkpoint");
        let segments = |generation: u64| -> Vec<String> {
            generation::load_manifest(&generation::manifest_path(&dir, generation))
                .expect("manifest")
                .files
                .into_iter()
                .map(|file| file.path)
                .filter(|path| path.starts_with("segment-"))
                .collect()
        };
        assert_eq!(segments(before).len(), 2);

        let maintainer = std::sync::Arc::clone(&store);
        probe::on_hash(Box::new(move || {
            maintainer.expire_before(1_000_000).expect("expire");
            maintainer.checkpoint().expect("checkpoint");
        }));
        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        let live = store.live_generation();
        assert!(live > before, "expiry and a checkpoint landed mid-check");
        assert_eq!(
            segments(live).len(),
            1,
            "one segment expired whole, one rewritten"
        );
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(
            verification["generation"]["id"],
            json!(live),
            "the pack checks the generation live once it has looked"
        );

        // Damage the maintenance did not cause still shows: a segment removed
        // by hand, with no checkpoint to account for it, is missing once a
        // digest reaches it again.
        let segment = segments(live).remove(0);
        fs::remove_file(dir.join(&segment)).expect("remove");
        store
            .ledger
            .advance_clock(EVIDENCE_VERIFY_REUSE + Duration::from_secs(1));
        store
            .evidence(None, None, None)
            .expect("a pack whose refresh redigests");
        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(false), "{verification}");
        assert!(
            verification["problems"]
                .to_string()
                .contains(&format!("{segment}: missing")),
            "{verification}"
        );
    }

    #[test]
    fn an_append_only_logs_digest_survives_appends_and_checkpoints_between_retries() {
        // An append-only log too long to digest in one budget, and before
        // every retry an append and a checkpoint: each new manifest names a
        // longer prefix of the same file. Together the retries digest more
        // than enough, so they must finish — the pack answers for the
        // generation its check began on (see `Pin`), each of its manifested
        // bytes digested once. And the next generation's check continues the
        // log's digest where that one stopped, rather than starting over.
        let dir = temp_dir("append-retries");
        let store = Store::open(&dir, budgeted()).expect("open");
        let note = |n: u64, size: usize| -> crate::annotations::Annotation {
            serde_json::from_value(json!({
                "trace_id": format!("t{n}"), "name": "note", "value": "x".repeat(size),
            }))
            .expect("annotation")
        };
        let notes = 10_u64;
        for n in 0..notes {
            store.annotate(note(n, 64 * 1024)).expect("annotate");
        }
        let manifest_of = |generation: u64| {
            generation::load_manifest(&generation::manifest_path(&dir, generation))
                .expect("manifest")
        };
        let first = store.checkpoint().expect("checkpoint");
        // Read now: checkpoints sweep the manifests of older generations.
        let checked = manifest_of(first);
        let before = probe::hashed_bytes();
        let bound = notes * 64 * 1024 / (FILE_CHECK_BYTES - FILE_OPEN_COST) + 10;
        let mut attempts = 0_u64;
        let answered = loop {
            attempts += 1;
            assert!(
                attempts <= bound,
                "retrying never finished the file check: {attempts} attempts"
            );
            match starved_pack(&store, None) {
                Ok(pack) => {
                    let verification = &pack["ledger"]["verification"];
                    assert_eq!(verification["intact"], json!(true), "{verification}");
                    break verification["generation"]["id"]
                        .as_u64()
                        .expect("generation");
                }
                Err(Error::DeadlineExceeded(detail)) => {
                    assert!(detail.contains("resumes"), "{detail}");
                }
                Err(other) => panic!("{other:?}"),
            }
            store.annotate(note(1_000 + attempts, 100)).expect("append");
            store.checkpoint().expect("checkpoint");
        };
        assert!(attempts > 2, "the log took several budgets: {attempts}");
        assert_eq!(answered, first, "the generation the check began on");
        assert_eq!(
            probe::hashed_bytes() - before,
            checked.files.iter().map(|file| file.bytes).sum::<u64>(),
            "every manifested byte digested once: no prefix was read twice"
        );

        // The generation live now: an unchanged entry costs nothing, a log
        // only what was appended past the prefix already digested, and any
        // other changed file its whole length.
        let live = store.live_generation();
        assert!(live > answered, "the store moved on while the check ran");
        let owed: u64 = manifest_of(live)
            .files
            .iter()
            .map(
                |file| match checked.files.iter().find(|old| old.path == file.path) {
                    Some(old) if unchanged(old, file) => 0,
                    Some(old)
                        if generation::is_append_only(&file.path) && old.bytes <= file.bytes =>
                    {
                        file.bytes - old.bytes
                    }
                    _ => file.bytes,
                },
            )
            .sum();
        let before = probe::hashed_bytes();
        let pack = store.evidence(None, None, None).expect("pack");
        let verification = &pack["ledger"]["verification"];
        assert_eq!(verification["intact"], json!(true), "{verification}");
        assert_eq!(verification["generation"]["id"], json!(live));
        assert_eq!(
            probe::hashed_bytes() - before,
            owed,
            "the log's digest continued across generations"
        );
    }

    /// The manifest entry a checkpoint would write for `path` under `dir`,
    /// as it is now.
    fn entry_of(dir: &Path, path: &str) -> generation::ManifestFile {
        let bytes = fs::read(dir.join(path)).expect("file");
        generation::ManifestFile {
            path: path.to_owned(),
            bytes: bytes.len() as u64,
            sha256: payload::sha256_hex(&bytes),
            modified_unix_ns: 0,
        }
    }

    /// A file check of `entries`, taken over from `previous` as a new
    /// generation's would be.
    fn verifier_of(
        generation: u64,
        entries: Vec<generation::ManifestFile>,
        previous: Option<FileVerifier>,
        ledger: &Ledger,
    ) -> FileVerifier {
        let mut built = FileVerifier::empty(generation, format!("m{generation}"), ledger.now());
        for entry in entries {
            built.push(entry, previous.as_ref());
        }
        if let Some(mut previous) = previous {
            built.adopt(&mut previous);
        }
        built
    }

    /// Replaces `path` under `dir` the way the store replaces a file: a new
    /// file renamed over it.
    fn rename_over(dir: &Path, path: &str, contents: &[u8]) {
        let staged = dir.join(".staged");
        fs::write(&staged, contents).expect("stage");
        fs::rename(&staged, dir.join(path)).expect("rename");
    }

    #[test]
    fn a_file_problem_stands_only_while_the_path_holds_the_file_read() {
        // A problem found off the maintenance lock is checked again under
        // it: it stands while the live generation is the one checked and
        // the path holds what was read, or still nothing.
        let dir = temp_dir("confirm");
        let ledger = Ledger::open(&dir).expect("ledger");
        let wanted = |path: &str, contents: &[u8]| generation::ManifestFile {
            path: path.to_owned(),
            bytes: contents.len() as u64,
            sha256: payload::sha256_hex(contents),
            modified_unix_ns: 0,
        };
        fs::write(dir.join("segment-1.seg"), b"stale").expect("write");
        fs::write(dir.join("segment-2.seg"), b"intact").expect("write");
        let mut files = verifier_of(
            7,
            vec![
                wanted("segment-1.seg", b"replacement"),
                wanted("segment-2.seg", b"intact"),
                wanted("segment-3.seg", b"gone"),
            ],
            None,
            &ledger,
        );
        let check = |files: &mut FileVerifier| {
            assert_eq!(
                files.check_unchecked(&dir, None, &ledger).expect("check"),
                Walked::Reached
            );
        };
        check(&mut files);
        assert_eq!(files.failing.len(), 2, "segment-1 and segment-3");
        assert!(files.unconfirmed());

        // Another generation is live: nothing is confirmed about this one.
        assert_eq!(
            files.confirm(&dir, 8, CONFIRM_BATCH).expect("confirm"),
            Confirmed::Moved
        );
        assert!(files.unconfirmed());

        // segment-1 replaced since it was read: digested again, and it
        // verifies; segment-3 is still missing, which stands. A batch of one
        // settles one problem, in path order, and leaves the other waiting.
        rename_over(&dir, "segment-1.seg", b"replacement");
        assert_eq!(
            files.confirm(&dir, 7, 1).expect("confirm"),
            Confirmed::Changed
        );
        assert_eq!(files.unchecked.len(), 1);
        assert!(files.unconfirmed(), "segment-3 waits for the next batch");
        assert_eq!(files.confirm(&dir, 7, 1).expect("confirm"), Confirmed::Held);
        assert!(!files.unconfirmed());
        check(&mut files);
        assert!(!files.unconfirmed());
        let problems = files.result(0).problems;
        assert_eq!(problems, ["segment-3.seg: missing"], "{problems:?}");

        // An edit in place is the same file: its problem stands.
        let segment = files.find("segment-2.seg").expect("segment-2");
        fs::write(dir.join("segment-2.seg"), b"in*act").expect("edit");
        files.forget(segment);
        check(&mut files);
        assert_eq!(
            files.confirm(&dir, 7, CONFIRM_BATCH).expect("confirm"),
            Confirmed::Held
        );
        let problems = files.result(0).problems;
        assert_eq!(
            problems,
            ["segment-2.seg: digest mismatch", "segment-3.seg: missing"],
            "{problems:?}"
        );
    }

    #[test]
    fn an_append_only_log_continues_its_digest_only_over_an_unchanged_prefix() {
        let dir = temp_dir("prefix");
        let ledger = Ledger::open(&dir).expect("ledger");
        let name = "annotations.jsonl";
        let log = dir.join(name);
        let check = |files: &mut FileVerifier| -> u64 {
            let before = probe::hashed_bytes();
            assert_eq!(
                files.check_unchecked(&dir, None, &ledger).expect("check"),
                Walked::Reached
            );
            assert!(files.failing.is_empty(), "{:?}", files.result(0).problems);
            probe::hashed_bytes() - before
        };
        fs::write(&log, "a\n".repeat(1000)).expect("write");
        let mut files = verifier_of(1, vec![entry_of(&dir, name)], None, &ledger);
        assert_eq!(check(&mut files), 2000);

        // Appended to and checkpointed: only what was appended is read.
        let mut file = OpenOptions::new().append(true).open(&log).expect("open");
        file.write_all("b\n".repeat(500).as_bytes())
            .expect("append");
        let mut files = verifier_of(2, vec![entry_of(&dir, name)], Some(files), &ledger);
        assert_eq!(check(&mut files), 1000);

        // Rewritten and renamed over the path, as expiry and erasures
        // rewrite a log: another file, digested from byte 0.
        rename_over(&dir, name, "c\n".repeat(2000).as_bytes());
        let mut files = verifier_of(3, vec![entry_of(&dir, name)], Some(files), &ledger);
        assert_eq!(check(&mut files), 4000);

        // Edited in place where the kept digest already read, then appended
        // to: the continued digest disagrees with the manifest's, and the
        // log is digested again from byte 0 rather than reported.
        let mut file = OpenOptions::new().write(true).open(&log).expect("open");
        file.write_all(b"C").expect("edit in place");
        let mut file = OpenOptions::new().append(true).open(&log).expect("open");
        file.write_all("d\n".repeat(10).as_bytes()).expect("append");
        let mut files = verifier_of(4, vec![entry_of(&dir, name)], Some(files), &ledger);
        assert_eq!(check(&mut files), 20 + 4020);
    }

    #[test]
    fn a_manifest_taken_in_a_value_at_a_time_decodes_as_one_taken_whole() {
        // The pack's resumable decode must accept exactly the manifests a
        // whole decode accepts, and refuse the ones it refuses.
        let whole = |text: &str| -> std::result::Result<Vec<String>, ()> {
            let manifest: generation::Manifest = serde_json::from_str(text).map_err(drop)?;
            Ok(manifest.files.into_iter().map(|file| file.path).collect())
        };
        let stepped = |text: &str| -> std::result::Result<Vec<String>, ()> {
            let mut built = FileVerifier::empty(1, String::new(), Instant::now());
            let mut parse = ManifestParse::default();
            while !parse
                .step(text.as_bytes(), &mut built, None)
                .map_err(drop)?
            {}
            Ok(built
                .files
                .iter()
                .map(|state| state.entry.path.clone())
                .collect())
        };
        let file = |path: &str| json!({"path": path, "bytes": 1, "sha256": digest(1), "modified_unix_ns": 5});
        let pretty = serde_json::to_string_pretty(&generation::Manifest {
            generation: 3,
            created_unix_ns: 9,
            folded_through: generation::FoldedThrough::NONE,
            files: serde_json::from_value(json!([file("a"), file("b/c"), file("d")]))
                .expect("files"),
            segment_format: Some(7),
        })
        .expect("manifest");
        let folded = json!({"epoch": 1, "sequence": 2});
        let cases = [
            pretty,
            json!({"files": [file("x")], "extra": {"nested": [1, {"files": 2}]},
                   "folded_through": folded, "created_unix_ns": 9, "generation": 3})
            .to_string(),
            json!({"generation": 3, "created_unix_ns": 9, "folded_through": folded,
                   "files": [], "segment_format": null})
            .to_string(),
            // Refused by both.
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[],"files":[]}"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"files":[]}"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[]} x"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[{"path":"a","bytes":1,"sha256":"s"},]}"#.to_owned(),
            r#"{"generation":-3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[]}"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":{}}"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[],"segment_format":7,"segment_format":7}"#.to_owned(),
            r#"{"generation":3,"created_unix_ns":9,"folded_through":{"epoch":1,"sequence":2},"files":[{"path":"a"}]}"#.to_owned(),
            r#"{"generation":3 "created_unix_ns":9}"#.to_owned(),
            String::new(),
        ];
        for (n, case) in cases.iter().enumerate() {
            assert_eq!(stepped(case), whole(case), "case {n}: {case}");
            assert_eq!(whole(case).is_ok(), n < 3, "case {n} is a {} case", n < 3);
        }
    }
}
