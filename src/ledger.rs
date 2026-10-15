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
//! appends are not serialized by the maintenance lock a verification holds,
//! and reading to EOF could take in half a line mid-append.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
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

/// How long an evidence pack may reuse a file verification of the SAME live
/// generation. Re-digesting every manifested byte is O(store); a pack is
/// readable by bound (tenant) credentials, and without reuse each of their
/// requests would buy a full-store re-read. The pack reports when its file
/// check ran (`verified_at_ns`), and `GET /v1/ledger/verify` never reuses.
const EVIDENCE_VERIFY_REUSE: Duration = Duration::from_secs(60);

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
    /// The last file verification, for evidence packs to reuse briefly; see
    /// [`EVIDENCE_VERIFY_REUSE`]. Leaf-level.
    file_check: Mutex<Option<FileCheck>>,
    /// Appends that failed since the ledger last caught up. Leaf-level.
    failures: Mutex<Failures>,
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
    /// `(erasure_id, subject_digest, generation)` of every erasure entry —
    /// one publication of one erasure. The digest guards against an id
    /// reused by another store's lineage (a restore of its backup into this
    /// directory); the generation tells a re-application after a rollback
    /// restore from the publication already ledgered.
    erasures: HashSet<(u64, String, u64)>,
}

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
                    .insert((*erasure_id, subject_digest.clone(), *generation));
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

/// One file verification result, kept for reuse.
#[derive(Clone, Debug)]
struct FileCheck {
    generation: u64,
    at_ns: u64,
    at: Instant,
    problems: Vec<String>,
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
            file_check: Mutex::new(None),
            failures: Mutex::new(Failures::default()),
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
            .contains(&(erasure_id, digest.clone(), generation))
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
}

// ------------------------------------------------------------ verification

/// The result of verifying the ledger and the live generation against it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verification {
    /// True when every head recomputes and links, the latest checkpoint names
    /// the live generation's manifest, every manifested file verifies, every
    /// ledgered erasure still holds in the live tombstone log, and no ledger
    /// append is outstanding.
    pub intact: bool,
    /// Entry lines in the ledger, readable or not.
    pub entries: u64,
    /// The recorded head of the last readable entry.
    pub head: Option<String>,
    /// Every discrepancy, each naming the entry (`seq N`) or file it is about.
    pub problems: Vec<String>,
    /// The live generation, checked against the chain and against its files.
    pub generation: GenerationCheck,
    /// Who each of `problems` concerns, index for index — so a tenant's
    /// evidence pack can keep its own, summarize the store's files, and
    /// withhold other tenants'.
    #[serde(skip)]
    scopes: Vec<Scope>,
}

/// Who one verification problem concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    /// The chain or the store as a whole: shown to every reader.
    Store,
    /// A manifested file: store-global names (segments, content-addressed
    /// payload paths), summarized as a count for a tenant.
    File,
    /// A ledgered erasure of this tenant (`None`: a store-global payload
    /// subject), shown only to that tenant and to unbound readers.
    Tenant(Option<String>),
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
    /// When the file check ran, nanoseconds since the Unix epoch.
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

    /// The verification as `tenant`'s evidence pack carries it. File
    /// problems name store-global files, so they collapse into a count;
    /// problems about another tenant's erasures (or a store-global payload
    /// erasure) collapse into a count that says nothing of whose. `intact`
    /// is unchanged: a pack never claims a store intact that is not.
    fn scoped(self, tenant: &str) -> Self {
        let mut problems = Vec::with_capacity(self.problems.len());
        let mut scopes = Vec::with_capacity(self.scopes.len());
        let mut files = 0_usize;
        let mut withheld = 0_usize;
        for (problem, scope) in self.problems.into_iter().zip(self.scopes) {
            match &scope {
                Scope::Store => {}
                Scope::Tenant(Some(owner)) if owner == tenant => {}
                Scope::File => {
                    files += 1;
                    continue;
                }
                Scope::Tenant(_) => {
                    withheld += 1;
                    continue;
                }
            }
            problems.push(problem);
            scopes.push(scope);
        }
        if files > 0 {
            problems.push(format!(
                "generation {}: {files} file(s) fail verification",
                self.generation.id
            ));
            scopes.push(Scope::File);
        }
        if withheld > 0 {
            problems.push(format!(
                "{withheld} problem(s) about data outside this tenant are withheld from this pack"
            ));
            scopes.push(Scope::Store);
        }
        Self {
            problems,
            scopes,
            ..self
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    match value {
        true => "yes",
        false => "NO",
    }
}

/// What walking the chain found.
#[derive(Debug, Default)]
struct ChainReport {
    entries: u64,
    head: Option<String>,
    /// `(seq, generation, manifest_sha256)` of the last checkpoint entry.
    last_checkpoint: Option<(u64, u64, String)>,
    /// The latest erasure entry per subject digest, for the cross-check
    /// against the live tombstone log.
    erasures: HashMap<String, LedgeredErasure>,
    problems: Vec<String>,
}

/// One erasure entry, as the cross-check needs it.
#[derive(Debug)]
struct LedgeredErasure {
    seq: u64,
    erasure_id: u64,
    subject_kind: String,
    tenant: Option<String>,
}

/// Recomputes every head in the first `length` bytes of the ledger at `path`
/// (the whole file when `None`) and checks each link.
///
/// Works on the generic JSON of each line, never a decoded [`Entry`]: a field
/// added by hand must change the recomputed head, and decoding into a typed
/// struct would silently drop it. Problems name the entry by `seq` where the
/// line says one, else by line number.
fn verify_chain(path: &Path, length: Option<u64>) -> Result<ChainReport> {
    let mut report = ChainReport::default();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report.problems.push(format!("{LOG_NAME} is missing"));
            return Ok(report);
        }
        Err(error) => return Err(Error::Io(error)),
    };
    let mut reader = BufReader::new(file.take(length.unwrap_or(u64::MAX)));
    let mut line = Vec::new();
    let mut line_number = 0_u64;
    // What the next entry must say, or `None` after a line that could not be
    // read — the chain cannot be followed across it, and reporting every
    // later entry as broken would bury the one real finding.
    let mut expected: Option<(u64, String)> = Some((0, GENESIS_PREV.to_owned()));
    // Recorded heads by seq, for checking anchors; only a contiguous prefix
    // is kept, so a hand-edited huge seq cannot force a huge allocation.
    let mut heads: Vec<String> = Vec::new();
    let mut genesis_seen = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        line_number += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            report.problems.push(format!("line {line_number}: blank"));
            continue;
        }
        report.entries += 1;
        let entry = match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(map)) => map,
            _ => {
                report.problems.push(format!(
                    "line {line_number}: not a JSON object; the chain cannot be followed past it"
                ));
                expected = None;
                continue;
            }
        };
        let Some(seq) = entry.get("seq").and_then(Value::as_u64) else {
            report
                .problems
                .push(format!("line {line_number}: no integer seq"));
            expected = None;
            continue;
        };
        let name = format!("seq {seq}");
        let recorded_head = entry.get("head").and_then(Value::as_str).map(str::to_owned);
        let prev = entry.get("prev").and_then(Value::as_str);
        if let Some((want_seq, want_prev)) = &expected {
            if seq != *want_seq {
                report.problems.push(format!(
                    "{name} (line {line_number}): out of order, seq {want_seq} was expected"
                ));
            } else if prev != Some(want_prev.as_str()) {
                report.problems.push(format!(
                    "{name}: prev does not match the head of the entry before it"
                ));
            }
        }
        let recomputed = compute_head(prev.unwrap_or(""), &Value::Object(entry.clone()));
        match &recorded_head {
            None => report.problems.push(format!("{name}: no head")),
            Some(head) if *head != recomputed => report.problems.push(format!(
                "{name}: head does not match its contents (recorded {head}, recomputed {recomputed})"
            )),
            Some(_) => {}
        }
        check_body(&entry, seq, &heads, &mut genesis_seen, &mut report.problems);
        match entry.get("kind").and_then(Value::as_str) {
            Some("checkpoint") => {
                if let (Some(generation), Some(manifest)) = (
                    entry.get("generation").and_then(Value::as_u64),
                    entry.get("manifest_sha256").and_then(Value::as_str),
                ) {
                    report.last_checkpoint = Some((seq, generation, manifest.to_owned()));
                }
            }
            Some("erasure") => {
                if let (Some(digest), Some(erasure_id)) = (
                    entry.get("subject_digest").and_then(Value::as_str),
                    entry.get("erasure_id").and_then(Value::as_u64),
                ) {
                    report.erasures.insert(
                        digest.to_owned(),
                        LedgeredErasure {
                            seq,
                            erasure_id,
                            subject_kind: entry
                                .get("subject_kind")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_owned(),
                            tenant: entry
                                .get("tenant")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        },
                    );
                }
            }
            _ => {}
        }
        let head = recorded_head.unwrap_or(recomputed);
        if seq == heads.len() as u64 {
            heads.push(head.clone());
        }
        expected = Some((seq.saturating_add(1), head.clone()));
        report.head = Some(head);
    }
    if !genesis_seen {
        report.problems.push("no genesis entry at seq 0".to_owned());
    }
    Ok(report)
}

/// The kind-specific checks: required fields present with the right types,
/// genesis first and only once, anchors naming a real earlier head.
fn check_body(
    entry: &Map<String, Value>,
    seq: u64,
    heads: &[String],
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
                Some(head) if head == target_head => {}
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

/// Subject digests of every SETTLED erasure in a tombstone log's statuses.
fn settled_subjects(statuses: &[erasure::ErasureStatus]) -> HashSet<String> {
    statuses
        .iter()
        .filter(|status| status.settle.is_some())
        .map(|status| subject_digest(&status.erase.subject))
        .collect()
}

/// [`settled_subjects`] read straight from `directory`'s tombstone log,
/// without opening it: the offline verification must not heal or append.
/// Lines that do not parse are skipped — a damaged tombstone log is the
/// file check's to report, and a skipped settle can only add a problem
/// here, never hide one.
fn settled_subjects_offline(directory: &Path) -> Result<HashSet<String>> {
    let contents = match fs::read(directory.join(erasure::LOG_NAME)) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(Error::Io(error)),
    };
    let mut subjects: HashMap<u64, String> = HashMap::new();
    let mut settled: HashSet<u64> = HashSet::new();
    for line in contents.split(|byte| *byte == b'\n') {
        let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(id) = record.get("id").and_then(Value::as_u64) else {
            continue;
        };
        match record.get("op").and_then(Value::as_str) {
            Some("erase") => {
                let subject = record
                    .get("subject")
                    .cloned()
                    .and_then(|subject| serde_json::from_value::<erasure::Subject>(subject).ok());
                if let Some(subject) = subject {
                    subjects.insert(id, subject_digest(&subject));
                }
            }
            Some("settle") => {
                settled.insert(id);
            }
            _ => {}
        }
    }
    Ok(subjects
        .into_iter()
        .filter(|(id, _)| settled.contains(id))
        .map(|(_, digest)| digest)
        .collect())
}

/// What [`assemble`] checks the chain against, besides the files.
struct LiveFacts<'a> {
    /// The generation `CURRENT` names.
    generation: u64,
    /// The file check of that generation.
    files: &'a FileCheck,
    /// Subject digests of every settled erasure in the live tombstone log,
    /// read AFTER the chain's prefix was fixed: an erasure entry is appended
    /// only once its settle is durable, so every erasure in the prefix is
    /// already in this set unless the log lost it.
    settled: &'a HashSet<String>,
    /// An append failure not yet caught up, if any.
    failure: Option<Failure>,
}

/// Verifies the first `length` bytes of the ledger in `directory` (all of it
/// when `None`) against the live facts.
fn assemble(directory: &Path, length: Option<u64>, live: LiveFacts<'_>) -> Result<Verification> {
    let chain = verify_chain(&directory.join(LOG_NAME), length)?;
    let generation = live.generation;
    let manifest_sha256 = manifest_digest(directory, generation)?;
    let mut problems = chain.problems;
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
            problems.push(format!(
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
            problems.push(format!(
                "seq {seq}: the latest checkpoint entry names generation {ledgered} (manifest \
                 {digest}), but the live generation is {generation} (manifest \
                 {manifest_sha256})"
            ));
            false
        }
        None => {
            problems.push(format!(
                "no checkpoint entry; the live generation {generation} is not ledgered"
            ));
            false
        }
    };
    let mut scopes = vec![Scope::Store; problems.len()];

    // Every ledgered erasure must still hold. The tombstone log never
    // rotates, so the one ordinary way for a settled erasure to leave it is
    // a restore of a pin taken before it — which also brings the subject's
    // data back, and is exactly what an auditor must not miss. Ordered by
    // seq so the report reads in chain order.
    let mut undone: Vec<&LedgeredErasure> = chain
        .erasures
        .iter()
        .filter(|(digest, _)| !live.settled.contains(digest.as_str()))
        .map(|(_, erasure)| erasure)
        .collect();
    undone.sort_by_key(|erasure| erasure.seq);
    for erasure in undone {
        problems.push(format!(
            "seq {}: erasure {} ({} subject) is ledgered, but the live tombstone log holds no \
             settled erasure of that subject. Restoring a pin taken before an erasure undoes \
             it and brings the subject's data back; re-apply the erasure",
            erasure.seq, erasure.erasure_id, erasure.subject_kind
        ));
        scopes.push(Scope::Tenant(erasure.tenant.clone()));
    }

    if let Some(failure) = &live.failure {
        problems.push(format!(
            "{} ledger append(s) failed since {} (most recently, {}). What they would have \
             recorded is ledgered at the next checkpoint or open",
            failure.count, failure.first_at_ns, failure.last_error
        ));
        scopes.push(Scope::Store);
    }

    for problem in &live.files.problems {
        problems.push(format!("generation {generation}: {problem}"));
        scopes.push(Scope::File);
    }
    Ok(Verification {
        intact: problems.is_empty(),
        entries: chain.entries,
        head: chain.head,
        problems,
        generation: GenerationCheck {
            id: generation,
            manifest_sha256,
            matches_ledger,
            files_verified: live.files.problems.is_empty(),
            verified_at_ns: live.files.at_ns,
        },
        scopes,
    })
}

/// Re-digests every file generation `live`'s manifest names.
fn check_files(directory: &Path, live: u64) -> Result<FileCheck> {
    let manifest = generation::load_manifest(&generation::manifest_path(directory, live))?;
    let problems = generation::verify_against(directory, &manifest)?;
    Ok(FileCheck {
        generation: live,
        at_ns: crate::unix_now_ns(),
        at: Instant::now(),
        problems,
    })
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
    let files = check_files(directory, live)?;
    // The directory lock excludes every writer, so neither the chain nor the
    // tombstone log can move while they are read: the whole file is the
    // chain, and the order of the two reads does not matter.
    let settled = settled_subjects_offline(directory)?;
    assemble(
        directory,
        None,
        LiveFacts {
            generation: live,
            files: &files,
            settled: &settled,
            failure: None,
        },
    )
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
    /// tombstone log, and reports any ledger append not yet caught up.
    ///
    /// O(store bytes), like [`Self::verify_generation`], and under the same
    /// maintenance lock: no compaction, expiry or checkpoint can move files
    /// or the chain's checkpoint entries mid-check. Erasure and anchor
    /// appends do not take that lock; the chain is read up to a snapshot
    /// taken under it, so they land past what is checked. Note that
    /// compaction and expiry replace files BETWEEN checkpoints, so the live
    /// manifest can name files that are gone until the next checkpoint
    /// publishes; those report as missing.
    pub fn verify_ledger(&self) -> Result<Verification> {
        self.verify_snapshot(None)
            .map(|(verification, _)| verification)
    }

    /// [`Self::verify_ledger`], reusing a file check of the same live
    /// generation that is at most `reuse` old, and returning the snapshot
    /// the chain was read up to. The chain, the manifest digest and the
    /// tombstone log are always re-read.
    fn verify_snapshot(&self, reuse: Option<Duration>) -> Result<(Verification, Snapshot)> {
        let _maintenance = self.lock_maintenance()?;
        // Under the maintenance lock, so no checkpoint entry can land between
        // this and reading `CURRENT`; erasure and anchor appends may, and
        // the snapshot leaves them out.
        let snapshot = self.ledger.snapshot()?;
        let live = self.live_generation();
        let cached = match reuse {
            Some(reuse) => self
                .ledger
                .file_check
                .lock()
                .map_err(|_| Error::LockPoisoned("ledger file check"))?
                .clone()
                .filter(|check| check.generation == live && check.at.elapsed() <= reuse),
            None => None,
        };
        let files = match cached {
            Some(files) => files,
            None => {
                let files = check_files(&self.directory, live)?;
                *self
                    .ledger
                    .file_check
                    .lock()
                    .map_err(|_| Error::LockPoisoned("ledger file check"))? = Some(files.clone());
                files
            }
        };
        // Read after the snapshot: see `LiveFacts::settled`.
        let settled = settled_subjects(&self.erasures()?);
        let verification = assemble(
            &self.directory,
            Some(snapshot.len),
            LiveFacts {
                generation: live,
                files: &files,
                settled: &settled,
                failure: self.ledger.outstanding_failure(),
            },
        )?;
        Ok((verification, snapshot))
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
    pub fn evidence(
        &self,
        tenant: Option<&str>,
        since_ns: Option<u64>,
        until_ns: Option<u64>,
    ) -> Result<Value> {
        let started = Instant::now();
        let generated_at_ns = crate::unix_now_ns();
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

        let (verification, snapshot) = self.verify_snapshot(Some(EVIDENCE_VERIFY_REUSE))?;
        let verification = match tenant {
            Some(tenant) => verification.scoped(tenant),
            None => verification,
        };
        let head = snapshot.head;
        let mut entries = Vec::new();
        let mut entries_truncated = false;
        // Lines the pack cannot place in the window: not a JSON object, or
        // no integer seq or at_ns. Counted, never silently dropped, so a
        // damaged chain's pack does not read as complete; the verification
        // names each one.
        let mut entries_unreadable = 0_u64;
        self.ledger.scan_lines(snapshot.len, |entry| {
            let Some(entry) = entry else {
                entries_unreadable += 1;
                return true;
            };
            let (Some(_), Some(at)) = (
                entry.get("seq").and_then(Value::as_u64),
                entry.get("at_ns").and_then(Value::as_u64),
            ) else {
                entries_unreadable += 1;
                return true;
            };
            if at < since || at > until {
                return true;
            }
            // Past the cap the scan goes on, counting: `entries_unreadable`
            // covers the whole chain, not just the part that fit.
            match entries.len() == EVIDENCE_MAX_ENTRIES {
                true => entries_truncated = true,
                false => entries.push(scope_entry(Value::Object(entry), tenant)),
            }
            true
        })?;

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
                "entries_truncated": entries_truncated,
                "entries_unreadable": entries_unreadable,
                "verification": verification,
            },
            "retention": self.retention_for(tenant)?,
            "usage": self.evidence_usage(tenant)?,
            "activity": self.evidence_activity(tenant, since, until)?,
            "erasures": self.evidence_erasures(tenant, since, until, started)?,
        });
        let digest = payload::sha256_hex(evals::canonical_json(&pack).as_bytes());
        if let Value::Object(map) = &mut pack {
            map.insert("digest".to_owned(), Value::String(digest));
        }
        Ok(pack)
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
    /// per-UTC-day breakdown, from one deadline-bounded fold. Distinct traces
    /// and sessions are counted by a 128-bit hash of `(tenant, id)`, which
    /// keeps a large window's memory at 16 bytes per distinct id.
    fn evidence_activity(&self, tenant: Option<&str>, since: u64, until: u64) -> Result<Value> {
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
        self.fold_spans(&filter, |span| {
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

    /// Erasures published (settled) in the window and visible to `tenant`,
    /// each with its receipt's verdict while the receipt budget lasts.
    ///
    /// `conclusive` and `result` are the receipt's own fields; both are
    /// `null` for an erasure past [`EVIDENCE_MAX_RECEIPTS`] or past the query
    /// deadline, which the pack states rather than guesses.
    fn evidence_erasures(
        &self,
        tenant: Option<&str>,
        since: u64,
        until: u64,
        started: Instant,
    ) -> Result<Vec<Value>> {
        let budget = self.config.query_deadline;
        let mut receipts = 0_usize;
        let mut out = Vec::new();
        for status in self.erasures()? {
            let Some(settle) = &status.settle else {
                continue;
            };
            if settle.settled_unix_ns < since || settle.settled_unix_ns > until {
                continue;
            }
            if tenant.is_some() && status.erase.subject.tenant() != tenant {
                continue;
            }
            let in_budget = receipts < EVIDENCE_MAX_RECEIPTS
                && budget.map_or(true, |budget| started.elapsed() < budget);
            let (conclusive, result) = match in_budget {
                true => {
                    receipts += 1;
                    let receipt = self.verify_erasure(status.erase.id)?;
                    (
                        Value::Bool(receipt.conclusive),
                        Value::String(receipt.result),
                    )
                }
                false => (Value::Null, Value::Null),
            };
            out.push(json!({
                "id": status.erase.id,
                "subject_kind": subject_kind(&status.erase.subject),
                "published_at_ns": settle.settled_unix_ns,
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
        assert!(report.problems.is_empty(), "{:?}", report.problems);
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
        assert!(report.problems.is_empty(), "{:?}", report.problems);
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
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(report.problems[0].starts_with("seq 1: head does not match"));

        // A store opened over the damage keeps working: it appends after the
        // last readable entry, and the damage stays reported.
        let ledger = Ledger::open(&dir).expect("open over damage");
        ledger.record_checkpoint(3, digest(3)).expect("cp");
        let report = verify_chain(&dir.join(LOG_NAME), None).expect("verify");
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
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
            report.problems.iter().any(|p| p.starts_with("seq 1: head")),
            "{:?}",
            report.problems
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
                .problems
                .iter()
                .any(|p| p.starts_with("seq 3 (line 3): out of order")),
            "{:?}",
            report.problems
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
        assert!(report.problems.is_empty(), "{:?}", report.problems);
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
        let scoped = verification.clone().scoped("acme");
        assert!(!scoped.intact);
        assert_eq!(undone(&scoped.problems), 1, "{:?}", scoped.problems);
        assert!(
            scoped
                .problems
                .iter()
                .any(|problem| problem.starts_with("1 problem(s) about data outside this tenant")),
            "{:?}",
            scoped.problems
        );
        assert!(!format!("{:?}", scoped.problems).contains("bigco"));

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
        assert!(report.problems.is_empty(), "{:?}", report.problems);
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
}
