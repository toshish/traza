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
//! formula is reproducible with any language's standard JSON library —
//! `docs/guide/ledger.md` has a ten-line script, and that reproducibility is
//! the point: an auditor must not need this code to check this code's claims.
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
//! Verification is what reports a break, naming the entry.
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

use std::collections::{BTreeMap, HashSet};
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
const EVIDENCE_MAX_ENTRIES: usize = 10_000;

/// Ceiling on the erasure receipts one evidence pack runs. Each receipt is a
/// real verification walk, so a window with many erasures would otherwise
/// make one request cost many walks; past this, `conclusive` is `null` and
/// `GET /v1/erasures/{id}/verify` answers for that erasure.
const EVIDENCE_MAX_RECEIPTS: usize = 32;

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

/// 32 hex characters naming a new chain.
///
/// Unique, not secret: two stores must not share an id, and nothing is
/// authenticated by it. OS randomness where the platform exposes it as a
/// file, always mixed with std's per-process random hasher keys, the clock,
/// the pid and the path, so a platform without `/dev/urandom` still gets a
/// distinct id — no dependency is needed for that.
fn fresh_store_id(directory: &Path) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut seed = Vec::with_capacity(64);
    if let Ok(mut file) = File::open("/dev/urandom") {
        let mut buffer = [0u8; 32];
        if file.read_exact(&mut buffer).is_ok() {
            seed.extend_from_slice(&buffer);
        }
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(directory.as_os_str().as_encoded_bytes());
    seed.extend_from_slice(&hasher.finish().to_le_bytes());
    seed.extend_from_slice(&crate::unix_now_ns().to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    payload::sha256_hex(&seed)[..32].to_owned()
}

// --------------------------------------------------------------------- log

/// The ledger file plus what appending needs to know about its tip.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    /// Leaf-level: taken briefly, never while acquiring another engine lock.
    /// Held across an append's write and fsync, which is what serializes
    /// appends and keeps a copy or a read from seeing half a line.
    tip: Mutex<Tip>,
    /// The last file verification, for evidence packs to reuse briefly; see
    /// [`EVIDENCE_VERIFY_REUSE`]. Leaf-level.
    file_check: Mutex<Option<FileCheck>>,
}

#[derive(Debug, Default)]
struct Tip {
    /// The last readable entry, or `None` for an empty ledger.
    last: Option<(u64, String, u64)>,
    store_id: Option<String>,
    /// `manifest_sha256` of the latest checkpoint entry.
    last_manifest: Option<String>,
    /// `(erasure_id, subject_digest)` of every erasure entry. Keyed by the
    /// digest too, so an erasure id reused by a different lineage (a restore
    /// of another store's backup into this directory) is not mistaken for
    /// one already ledgered.
    erasures: HashSet<(u64, String)>,
}

impl Tip {
    fn absorb(&mut self, entry: &Entry) {
        self.last = Some((entry.seq, entry.head.clone(), entry.at_ns));
        match &entry.body {
            Body::Genesis { store_id } => {
                if self.store_id.is_none() {
                    self.store_id = Some(store_id.clone());
                }
            }
            Body::Checkpoint {
                manifest_sha256, ..
            } => self.last_manifest = Some(manifest_sha256.clone()),
            Body::Erasure {
                erasure_id,
                subject_digest,
                ..
            } => {
                self.erasures.insert((*erasure_id, subject_digest.clone()));
            }
            Body::Anchor { .. } => {}
        }
    }
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
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let path = directory.join(LOG_NAME);
        let mut tip = Tip::default();
        if path.exists() {
            let mut reader = BufReader::new(File::open(&path)?);
            let mut valid_len = 0_u64;
            let mut line = Vec::new();
            let mut torn = false;
            let mut unterminated_tail = false;
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                let terminated = line.ends_with(b"\n");
                match serde_json::from_slice::<Entry>(&line) {
                    Ok(entry) => {
                        tip.absorb(&entry);
                        unterminated_tail = !terminated;
                    }
                    Err(_) if !terminated => {
                        torn = true;
                        break;
                    }
                    Err(_) => {}
                }
                valid_len += read as u64;
            }
            if torn {
                let file = OpenOptions::new().write(true).open(&path)?;
                file.set_len(valid_len)?;
                file.sync_all()?;
            } else if unterminated_tail {
                // A complete entry missing only its newline: terminate it,
                // or the next append would be glued onto the same line.
                let mut file = OpenOptions::new().append(true).open(&path)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
        }
        let ledger = Self {
            path,
            tip: Mutex::new(tip),
            file_check: Mutex::new(None),
        };
        {
            let mut tip = ledger.lock()?;
            if tip.last.is_none() {
                let store_id = fresh_store_id(directory);
                ledger.append_locked(&mut tip, Body::Genesis { store_id })?;
            }
        }
        Ok(ledger)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Tip>> {
        self.tip.lock().map_err(|_| Error::LockPoisoned("ledger"))
    }

    /// Appends `body` as the next entry, durably, under the held tip lock.
    fn append_locked(&self, tip: &mut Tip, body: Body) -> Result<Entry> {
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
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        let created = !self.path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let before = file.metadata()?.len();
        if let Err(error) = file
            .write_all(line.as_bytes())
            .and_then(|()| file.sync_all())
        {
            // A failed append (a full disk) must not leave half a line behind:
            // the next append would be glued onto it, and one I/O error would
            // read forever after as a broken chain. Best effort — if the
            // truncate fails too, open's torn-tail healing is the backstop.
            let _ = file.set_len(before);
            return Err(Error::Io(error));
        }
        if created {
            if let Some(directory) = self.path.parent() {
                crate::sync_directory(directory)?;
            }
        }
        tip.absorb(&entry);
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

    /// Ledgers a published erasure unless the chain already carries it.
    fn record_erasure(&self, erasure_id: u64, subject: &erasure::Subject) -> Result<Option<Entry>> {
        let digest = subject_digest(subject);
        let mut tip = self.lock()?;
        if tip.erasures.contains(&(erasure_id, digest.clone())) {
            return Ok(None);
        }
        self.append_locked(
            &mut tip,
            Body::Erasure {
                erasure_id,
                tenant: subject.tenant().map(str::to_owned),
                subject_kind: subject_kind(subject).to_owned(),
                subject_digest: digest,
            },
        )
        .map(Some)
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
        if !is_sha256_hex(&request.target_head) {
            return Err(Error::Invalid(
                "target_head is 64 lowercase hex characters".to_owned(),
            ));
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
            // Under the held tip lock the file is exactly the tip's prefix,
            // so its current length is the length to scan.
            false => self.head_at(request.target_seq, self.length()?)?,
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

    /// Streams every readable entry, as stored, to `visit` until it returns
    /// false. Reads only the prefix that existed when the scan began, under
    /// no lock: the file is append-only, so that prefix cannot change, and a
    /// long read must not hold up a checkpoint's append.
    fn scan(&self, visit: impl FnMut(Value) -> bool) -> Result<()> {
        // The tip lock only orders the length read after any append in
        // flight, so the prefix ends on a complete line; the scan itself runs
        // unlocked.
        let length = {
            let _tip = self.lock()?;
            self.length()?
        };
        self.scan_prefix(length, visit)
    }

    /// The ledger's current length in bytes; zero when it does not exist.
    fn length(&self) -> Result<u64> {
        match fs::metadata(&self.path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(Error::Io(error)),
        }
    }

    /// [`Self::scan`] over the first `length` bytes, for a caller that
    /// already holds the tip lock (the lock is not reentrant).
    fn scan_prefix(&self, length: u64, mut visit: impl FnMut(Value) -> bool) -> Result<()> {
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
            let Ok(entry) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if entry.is_object() && !visit(entry) {
                return Ok(());
            }
        }
    }

    fn head(&self) -> Result<LedgerHead> {
        let tip = self.lock()?;
        let (seq, head, at_ns) = tip.last.clone().unwrap_or((0, GENESIS_PREV.to_owned(), 0));
        Ok(LedgerHead {
            seq,
            head,
            store_id: tip.store_id.clone(),
            at_ns,
        })
    }

    /// Copies the ledger, whole, to `target` — under the tip lock, so the
    /// copy ends on a complete line.
    fn copy_to(&self, target: &Path) -> Result<()> {
        let _tip = self.lock()?;
        let length = fs::metadata(&self.path)?.len();
        generation::copy_prefix(&self.path, target, length)
    }
}

// ------------------------------------------------------------ verification

/// The result of verifying the ledger and the live generation against it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verification {
    /// True when every head recomputes and links, the latest checkpoint names
    /// the live generation's manifest, and every manifested file verifies.
    pub intact: bool,
    /// Entry lines in the ledger, readable or not.
    pub entries: u64,
    /// The recorded head of the last readable entry.
    pub head: Option<String>,
    /// Every discrepancy, each naming the entry (`seq N`) or file it is about.
    pub problems: Vec<String>,
    /// The live generation, checked against the chain and against its files.
    pub generation: GenerationCheck,
    /// How many of `problems`, at the end, are file problems — so a tenant's
    /// evidence pack can summarize them rather than name other tenants' files.
    #[serde(skip)]
    file_problems: usize,
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

    /// The verification as a tenant's evidence pack carries it: file
    /// problems name store-global files (segments, content-addressed payload
    /// paths), so they collapse into a count.
    fn scoped(mut self) -> Self {
        if self.file_problems > 0 {
            let keep = self.problems.len() - self.file_problems;
            self.problems.truncate(keep);
            self.problems.push(format!(
                "generation {}: {} file(s) fail verification",
                self.generation.id, self.file_problems
            ));
            self.file_problems = 1;
        }
        self
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
    problems: Vec<String>,
}

/// Recomputes every head in the ledger at `path` and checks each link.
///
/// Works on the generic JSON of each line, never a decoded [`Entry`]: a field
/// added by hand must change the recomputed head, and decoding into a typed
/// struct would silently drop it. Problems name the entry by `seq` where the
/// line says one, else by line number.
fn verify_chain(path: &Path) -> Result<ChainReport> {
    let mut report = ChainReport::default();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report.problems.push(format!("{LOG_NAME} is missing"));
            return Ok(report);
        }
        Err(error) => return Err(Error::Io(error)),
    };
    let mut reader = BufReader::new(file);
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
        if entry.get("kind").and_then(Value::as_str) == Some("checkpoint") {
            if let (Some(generation), Some(manifest)) = (
                entry.get("generation").and_then(Value::as_u64),
                entry.get("manifest_sha256").and_then(Value::as_str),
            ) {
                report.last_checkpoint = Some((seq, generation, manifest.to_owned()));
            }
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
            {
                problems.push(format!(
                    "{name}: erasure without erasure_id, tenant, subject_kind and subject_digest"
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

/// Verifies the ledger in `directory` against generation `live` and the file
/// check `files`.
fn assemble(directory: &Path, live: u64, files: &FileCheck) -> Result<Verification> {
    let chain = verify_chain(&directory.join(LOG_NAME))?;
    let manifest_sha256 = manifest_digest(directory, live)?;
    let mut problems = chain.problems;
    let matches_ledger = match &chain.last_checkpoint {
        Some((_, generation, digest)) if *generation == live && *digest == manifest_sha256 => true,
        Some((seq, generation, digest)) => {
            problems.push(format!(
                "seq {seq}: the latest checkpoint entry names generation {generation} \
                 (manifest {digest}), but the live generation is {live} (manifest \
                 {manifest_sha256})"
            ));
            false
        }
        None => {
            problems.push(format!(
                "no checkpoint entry; the live generation {live} is not ledgered"
            ));
            false
        }
    };
    let file_problems = files.problems.len();
    problems.extend(
        files
            .problems
            .iter()
            .map(|problem| format!("generation {live}: {problem}")),
    );
    Ok(Verification {
        intact: problems.is_empty(),
        entries: chain.entries,
        head: chain.head,
        problems,
        generation: GenerationCheck {
            id: live,
            manifest_sha256,
            matches_ledger,
            files_verified: files.problems.is_empty(),
            verified_at_ns: files.at_ns,
        },
        file_problems,
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
/// with [`Error::AlreadyOpen`]) and reads `CURRENT`, the manifest, the files
/// and the ledger as they are.
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
    assemble(directory, live, &files)
}

// ------------------------------------------------------------------- store

impl Store {
    /// Ledgers the generation just published, unless its manifest is the one
    /// already ledgered. Called by the checkpoint once `CURRENT` is durable.
    pub(crate) fn ledger_checkpoint(&self, generation: u64) -> Result<()> {
        let digest = manifest_digest(&self.directory, generation)?;
        self.ledger.record_checkpoint(generation, digest).map(drop)
    }

    /// Ledgers an erasure just published. Called once its settle record is
    /// durable.
    pub(crate) fn ledger_erasure(&self, erasure_id: u64, subject: &erasure::Subject) -> Result<()> {
        self.ledger.record_erasure(erasure_id, subject).map(drop)
    }

    /// Copies the ledger into a pin being built, beside its manifest.
    pub(crate) fn ledger_copy_into(&self, pin_dir: &Path) -> Result<()> {
        self.ledger.copy_to(&pin_dir.join(LOG_NAME))
    }

    /// The chain's tip.
    pub fn ledger_head(&self) -> Result<LedgerHead> {
        self.ledger.head()
    }

    /// Up to `limit` entries with `seq` greater than `after` (all entries when
    /// `after` is `None`), in file order, exactly as stored. Lines that do not
    /// parse are skipped here; [`Self::verify_ledger`] names them.
    pub fn ledger_entries(&self, after: Option<u64>, limit: usize) -> Result<Vec<Value>> {
        let mut entries = Vec::new();
        if limit == 0 {
            return Ok(entries);
        }
        self.ledger.scan(|entry| {
            let seq = entry.get("seq").and_then(Value::as_u64);
            if after.map_or(true, |after| seq.is_some_and(|seq| seq > after)) {
                entries.push(entry);
            }
            entries.len() < limit
        })?;
        Ok(entries)
    }

    /// Verifies the whole chain, checks that the latest checkpoint entry
    /// names the live generation's manifest, and re-digests every file that
    /// manifest names.
    ///
    /// O(store bytes), like [`Self::verify_generation`], and under the same
    /// maintenance lock: no compaction, expiry or checkpoint can move files
    /// or the chain mid-check. Note that compaction and expiry replace files
    /// BETWEEN checkpoints, so the live manifest can name files that are gone
    /// until the next checkpoint publishes; those report as missing.
    pub fn verify_ledger(&self) -> Result<Verification> {
        self.verify_ledger_reusing(None)
    }

    /// [`Self::verify_ledger`], reusing a file check of the same live
    /// generation that is at most `reuse` old. The chain and the manifest
    /// digest are always re-read.
    fn verify_ledger_reusing(&self, reuse: Option<Duration>) -> Result<Verification> {
        let _maintenance = self.lock_maintenance()?;
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
        assemble(&self.directory, live, &files)
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
    /// stays followable without disclosing whose erasure it was.
    ///
    /// `usage` is `null` in this build: the per-tenant usage counters are a
    /// separate feature, and this pack carries them when they exist.
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

        // Head first, then entries bounded by it: an append landing mid-pack
        // must not put an entry past the head the pack reports.
        let head = self.ledger_head()?;
        let mut entries = Vec::new();
        let mut entries_truncated = false;
        self.ledger.scan(|entry| {
            let seq = entry.get("seq").and_then(Value::as_u64).unwrap_or(u64::MAX);
            if seq > head.seq {
                return false;
            }
            let at = entry.get("at_ns").and_then(Value::as_u64).unwrap_or(0);
            if at < since || at > until {
                return true;
            }
            if entries.len() == EVIDENCE_MAX_ENTRIES {
                entries_truncated = true;
                return false;
            }
            entries.push(scope_entry(entry, tenant));
            true
        })?;
        let mut verification = self.verify_ledger_reusing(Some(EVIDENCE_VERIFY_REUSE))?;
        if tenant.is_some() {
            verification = verification.scoped();
        }

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
                "verification": verification,
            },
            "retention": self.retention_for(tenant),
            "usage": Value::Null,
            "activity": self.evidence_activity(tenant, since, until)?,
            "erasures": self.evidence_erasures(tenant, since, until, started)?,
        });
        let digest = payload::sha256_hex(evals::canonical_json(&pack).as_bytes());
        if let Value::Object(map) = &mut pack {
            map.insert("digest".to_owned(), Value::String(digest));
        }
        Ok(pack)
    }

    /// The retention window that applies to `tenant`, and where it comes
    /// from: a per-tenant override (`tenant`), the global window (`global`),
    /// or nothing configured (`none`). `ttl_seconds` is `null` whenever the
    /// effective policy keeps data forever — a `0` override included. A
    /// store-wide pack reports the global window.
    fn retention_for(&self, tenant: Option<&str>) -> Value {
        let global = self.config.ttl_seconds.filter(|ttl| *ttl > 0);
        match tenant.and_then(|tenant| self.config.tenant_ttl_seconds.get(tenant)) {
            Some(ttl) => json!({"ttl_seconds": (*ttl > 0).then_some(*ttl), "source": "tenant"}),
            None => match global {
                Some(ttl) => json!({"ttl_seconds": ttl, "source": "global"}),
                None => json!({"ttl_seconds": Value::Null, "source": "none"}),
            },
        }
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
/// only what following the chain needs.
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
pub(crate) fn open_reconciled(
    directory: &Path,
    live_generation: u64,
    erasures: &erasure::ErasureLog,
) -> Result<Ledger> {
    let ledger = Ledger::open(directory)?;
    ledger.record_checkpoint(
        live_generation,
        manifest_digest(directory, live_generation)?,
    )?;
    for status in erasures.list()? {
        if status.settle.is_some() {
            ledger.record_erasure(status.erase.id, &status.erase.subject)?;
        }
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
            let head = store.ledger_head()?;
            let entries = store.ledger_entries(after, limit)?;
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
                            json!({"error": "this credential is bound to a different tenant"}),
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

        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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
        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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

        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(report.problems[0].starts_with("seq 1: head does not match"));

        // A store opened over the damage keeps working: it appends after the
        // last readable entry, and the damage stays reported.
        let ledger = Ledger::open(&dir).expect("open over damage");
        ledger.record_checkpoint(3, digest(3)).expect("cp");
        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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
        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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
        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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
        assert!(matches!(
            ledger.anchor(request(0, "not-hex")),
            Err(Error::Invalid(_))
        ));
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
        let report = verify_chain(&dir.join(LOG_NAME)).expect("verify");
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
            .record_erasure(3, &subject)
            .expect("record")
            .expect("appended");
        assert!(ledger.record_erasure(3, &subject).expect("again").is_none());
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
            } => {
                assert_eq!(erasure_id, 3);
                assert_eq!(tenant.as_deref(), Some("acme"));
                assert_eq!(subject_kind, "session");
                assert_eq!(subject_digest, payload::sha256_hex(canonical.as_bytes()));
            }
            other => panic!("not an erasure entry: {other:?}"),
        }
        // A payload subject is store-global: its tenant is null.
        let payload_subject = erasure::Subject::Payload {
            reference: format!("sha256/{}", digest(7)),
        };
        let entry = ledger
            .record_erasure(4, &payload_subject)
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
                             "subject_digest": "e"});
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
}
