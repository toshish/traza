//! The v6 → v7 migrator: automatic at first open, resumable, and never woven
//! into the read path.
//!
//! The contract is the Migration section of `docs/segment-format.md`. In one
//! paragraph: `Store::open` on a v0.24.0 build converts a v6 store before
//! serving anything. Every v6 segment is decoded by the FROZEN decoder below,
//! its records re-derived from their payloads through the same span → record
//! derivation ingest uses, and re-encoded as v7 onto its SAME file name
//! (temp + fsync + rename — segment path order IS recency order, so a fresh
//! name would reorder last-write-wins). Rollup sidecars are rebuilt with
//! fresh bindings from the records already in hand. Payload blobs are
//! classified three ways — valid v7 is left alone, a raw file whose bytes
//! SHA-256 to its name is a v6 blob and is rewritten as `TRZBLOB1`, and a
//! file that is neither is refused per file rather than laundered into a
//! validly framed blob. Pins get the same two passes plus a manifest-digest
//! rewrite, because restore verifies a pin against those digests. Completion
//! is a checkpoint whose manifest declares the store format and whose digests
//! are a FULL RE-HASH — the incremental carry-over rule assumes segments are
//! immutable, and migration has just rewritten every one of them onto its
//! same name, so a carried digest would describe bytes that no longer exist.
//! `folded_through` is published unchanged: the migrator does not read the
//! WAL, and frames after the fold replay normally against the migrated store.
//!
//! Resumability is by construction, not by journal: every file conversion is
//! atomic, a v7 segment is recognized by its version word, a v7 blob by full
//! validation, and the completion checkpoint is the only bit of state. A
//! crash at any point leaves a store the next open finishes — any v6 segment
//! (live or pinned) restarts the full migration, and an all-v7 store with no
//! manifest declaration re-runs the idempotent blob pass, re-validates every
//! pin, and checkpoints. Pin re-validation deliberately does not trust the
//! version-word trigger: between a pin's file pass and its manifest rewrite
//! every file already reads as v7, so resume re-checks each pin's digests and
//! redoes the manifest rewrite where the files validate as v7 but the
//! digests disagree.
//!
//! There are no reads during migration; `Store::open` runs this before WAL
//! replay and before any maintenance, and serves nothing until the store
//! holds one format. The v6 decoder lives ONLY here — the live reader in
//! `src/segment.rs` speaks exactly one version.
//!
//! One cost is RAM, and it is stated rather than hidden: `migrate_segment`
//! converts a segment entirely in memory, with roughly FOUR times the
//! segment's size resident at peak — the raw v6 bytes, the re-derived
//! records (payload copies included), the parsed spans, and the encoded v7
//! output all overlap. Under the default 256 MiB compaction cap that peak is
//! about 1 GiB; a store whose `max_segment_bytes = 0` (uncapped) grew
//! multi-gigabyte segments needs commensurate memory to migrate them. An
//! OOM kill mid-segment is crash-safe like every other kill — nothing is
//! torn, the next open resumes — but it resumes INTO the same segment and
//! the same kill, which makes it the one failure the resume design cannot
//! make progress past; the way out is more memory, not a retry. A streaming
//! converter would remove the ceiling at the cost of a second encoder shape,
//! and is deliberately not built for a pre-release migration whose default
//! configuration never comes near the limit.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::{analytics, generation, payload, rollup_file, segment, sync_directory};
use crate::{Config, Error, Result, Span};

/// A migration failure is an open failure: named, actionable, and shaped like
/// the other refusals `Store::open` produces.
fn refuse(message: String) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Entry point, called by `Store::open` after crash recovery
/// (`remove_orphan_temps`, `recover_supersede_markers`) and before the live
/// manifest is trusted or the WAL replayed. Returns the live generation —
/// advanced past `live_generation` exactly when a completion checkpoint was
/// published.
///
/// The trigger rule, verbatim from the spec: any segment (live or pinned)
/// declaring v6 starts a full migration; all segments v7 but no manifest
/// declaration re-runs the idempotent blob pass, re-validates every pin, and
/// then checkpoints. Both branches run the same sequence — the segment pass
/// simply finds nothing to convert in the second — which is what makes a
/// crash at any point re-run from where it stopped.
pub(crate) fn run_at_open(directory: &Path, config: &Config, live_generation: u64) -> Result<u64> {
    // The legacy-JSONL guard `nothing_to_migrate` has, for the same reason:
    // a v1 store is not this migrator's to declare v7. Step aside so
    // `load_segments` refuses it by name (migrate with 0.3.x first), instead
    // of publishing a completion checkpoint over segments this build cannot
    // read — and sweeping the adoption manifest on the way out.
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file()
            && name.starts_with(crate::SEGMENT_PREFIX)
            && name.ends_with(crate::LEGACY_SEGMENT_SUFFIX)
        {
            return Ok(live_generation);
        }
    }

    let mut any_v6 = false;
    let mut unreadable: Vec<String> = Vec::new();
    for path in segment_files(directory)? {
        match segment_version(&path)? {
            Some(v6::VERSION) => any_v6 = true,
            Some(version) if version == segment::VERSION => {}
            // A foreign version, a foreign magic, or a truncated head: not
            // this migrator's to convert. Collected rather than acted on,
            // because what to do depends on the rest of the scan.
            Some(version) => unreadable.push(format!(
                "{}: declares segment format v{version}",
                path.display()
            )),
            None => unreadable.push(format!(
                "{}: too short or foreign magic — not a readable segment head",
                path.display()
            )),
        }
    }
    for pin in pin_dirs(directory)? {
        for path in segment_files(&pin)? {
            if segment_version(&path)? == Some(v6::VERSION) {
                any_v6 = true;
            }
        }
    }
    if !unreadable.is_empty() {
        if any_v6 {
            // A v6 store with a damaged file: converting around it would
            // bury the report, and stepping aside would be worse —
            // `load_segments` aborts on the FIRST unreadable segment in path
            // order, so a healthy v6 segment sorting earlier would draw a
            // version-mismatch refusal pointing away from the file that is
            // actually damaged. Refuse here, naming exactly what is wrong.
            return Err(refuse(format!(
                "cannot migrate: {} segment file(s) beside this store's v6 \
                 segments do not read as v6 or v7, and converting the store \
                 around them would bury the report. Restore each from a \
                 backup or remove it, then reopen and the migration will \
                 run:\n{}",
                unreadable.len(),
                unreadable.join("\n")
            )));
        }
        // No v6 anywhere: there is nothing to migrate around, and
        // `load_segments` owns the refusal — it names the file, and its
        // version advice is correct for a store with no v6 in it.
        return Ok(live_generation);
    }

    let manifest =
        generation::load_manifest(&generation::manifest_path(directory, live_generation))?;
    // Declared v7 and no v6 version word anywhere: the ordinary open. The
    // scan above is the whole per-open cost of the trigger.
    if !any_v6 && manifest.segment_format.is_some() {
        return Ok(live_generation);
    }

    // ---- live segment pass -------------------------------------------------
    // Sorted path order, each conversion atomic onto its own name. A segment
    // already v7 (a previous attempt got to it) is left exactly alone.
    for path in segment_files(directory)? {
        if segment_version(&path)? == Some(v6::VERSION) {
            migrate_segment(&path, config, true)?;
        }
    }
    sync_directory(directory)?;

    // ---- live blob pass ----------------------------------------------------
    let problems = migrate_blob_tree(&directory.join(payload::PAYLOAD_DIR))?;
    if !problems.is_empty() {
        return Err(refuse(format!(
            "migration refuses to rewrite {} payload file(s) that match neither \
             format — not a valid v7 blob, and the raw bytes do not SHA-256 to \
             the file name. Rewriting would launder unrecognized bytes into a \
             validly framed blob. Restore each file from a backup or remove it, \
             then reopen — the store serves nothing until then, the next open \
             resumes the migration from where this one stopped, and a pending \
             erasure settles after it. Every unrecognized file is listed, so one \
             pass covers them all:\n{}",
            problems.len(),
            problems.join("\n")
        )));
    }

    // ---- pins --------------------------------------------------------------
    for pin in pin_dirs(directory)? {
        migrate_pin(&pin, config)?;
    }

    // ---- completion checkpoint ---------------------------------------------
    complete(directory, live_generation, manifest.folded_through)
}

/// Whether `root` holds nothing the migrator would ever need to touch: no
/// segment files in any format but v7, no payload files, no pins. Used at
/// generation adoption so a store born on this build (or adopted with a
/// purely-v7 working set) is declared v7 in its first manifest and never
/// enters the migration path at all.
pub(crate) fn nothing_to_migrate(root: &Path) -> Result<bool> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Legacy JSONL segments are refused later; an adopted directory
        // holding one is certainly not "nothing to migrate".
        if entry.file_type()?.is_file()
            && name.starts_with(crate::SEGMENT_PREFIX)
            && name.ends_with(crate::LEGACY_SEGMENT_SUFFIX)
        {
            return Ok(false);
        }
    }
    for path in segment_files(root)? {
        if segment_version(&path)? != Some(segment::VERSION) {
            return Ok(false);
        }
    }
    if tree_has_files(&root.join(payload::PAYLOAD_DIR))? {
        return Ok(false);
    }
    if !pin_dirs(root)?.is_empty() {
        return Ok(false);
    }
    Ok(true)
}

/// The declared format version of one segment file: `Some(version)` when the
/// file begins with the segment magic, `None` for anything shorter or
/// foreign. Ten bytes read per file per open — the whole cost of the
/// version-word trigger.
fn segment_version(path: &Path) -> Result<Option<u16>> {
    use std::io::Read;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Io(error)),
    };
    let mut head = [0u8; 10];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    if filled < head.len() || head[..8] != segment::MAGIC {
        return Ok(None);
    }
    Ok(Some(u16::from_le_bytes([head[8], head[9]])))
}

/// Every `segment-*.seg` file directly under `dir`, in path order — which is
/// recency order, and therefore the deterministic order every pass runs in.
fn segment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(crate::SEGMENT_PREFIX) && name.ends_with(crate::SEGMENT_SUFFIX) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// Every pin directory under `pins/`, in path order. Dot-prefixed entries are
/// staging leftovers (`.{label}.pinning`), not pins.
fn pin_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pins = Vec::new();
    let entries = match fs::read_dir(root.join(generation::PINS_DIR)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(pins),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            pins.push(entry.path());
        }
    }
    pins.sort();
    Ok(pins)
}

/// Whether any regular file exists anywhere under `root`.
fn tree_has_files(root: &Path) -> Result<bool> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Converts one v6 segment file to v7, onto its SAME path.
///
/// The records are re-derived from their payloads: parse each payload as a
/// span, run it through the same `span_to_record` derivation ingest uses —
/// attributes, digests, and content text are all recoverable from the
/// payload, which is what makes this a re-encode rather than a lossy copy.
/// The v6 record's own attribute strings are decoded only to walk the bytes;
/// the derivation from the payload is the definition, exactly as it is for
/// the format's derivation invariant. (For records sealed under older
/// decodings — the pre-`$tenant` fixture — the two legitimately differ, and
/// the payload-derived answer is the current build's answer.)
///
/// `with_rollup` distinguishes the live pass (which writes a freshly bound
/// sidecar from the spans already decoded, so first query is not a rebuild
/// storm) from a pin's pass (pins hold no sidecars — they are derived caches
/// a manifest never lists). The write order of sidecar against segment
/// rename is deliberately unspecified: a stale-bound sidecar self-invalidates
/// and rebuilds on first use.
fn migrate_segment(path: &Path, config: &Config, with_rollup: bool) -> Result<()> {
    let bytes = fs::read(path)?;
    let decoded = v6::decode_segment(&bytes).map_err(|reason| {
        refuse(format!(
            "cannot migrate {}: {reason}. The file declares segment format v6 \
             but does not decode as one — it may be truncated, damaged, or \
             mis-stamped. Nothing was rewritten. Back up the directory first — \
             stop the server and copy it, or take a filesystem snapshot atomic \
             across the whole directory — then inspect the file before \
             changing anything. See docs/operations/durability.md#backups",
            path.display()
        ))
    })?;

    let mut records = Vec::with_capacity(decoded.len());
    let mut spans: Vec<Span> = Vec::with_capacity(decoded.len());
    for (ordinal, record) in decoded.iter().enumerate() {
        let span: Span = serde_json::from_slice(&record.payload).map_err(|error| {
            refuse(format!(
                "cannot migrate {}: record #{ordinal} (trace {:?}, timestamp \
                 {}) has a payload that does not parse as a span: {error}",
                path.display(),
                record.trace_id,
                record.timestamp
            ))
        })?;
        records.push(crate::span_to_record(&span)?);
        spans.push(span);
    }

    // The encoder enforces the 2^31 record bound itself and its `TooLarge`
    // names the record (trace id and timestamp); the wrap adds the one thing
    // the encoder cannot know — which segment — plus the operator's way out.
    let encoded =
        segment::encode_with(&records, config.content_index).map_err(|error| match &error {
            segment::Error::TooLarge(_) => refuse(format!(
                "cannot migrate {}: {error} — at or above the 2^31 record \
                 bound. No record is skipped silently; export and re-ingest \
                 the store without this record, or erase its subject first",
                path.display()
            )),
            _ => refuse(format!("cannot migrate {}: {error}", path.display())),
        })?;
    write_replacing(path, &encoded)?;

    if with_rollup {
        // Fresh binding for the freshly written bytes; pricing is the
        // store's, exactly as at seal. Best-effort like every sidecar write:
        // a missing sidecar is rebuilt on demand, never a wrong answer.
        let (min_start_ns, max_start_ns) =
            records.iter().fold((u64::MAX, 0u64), |(lo, hi), record| {
                (lo.min(record.timestamp), hi.max(record.timestamp))
            });
        let binding = rollup_file::Binding {
            segment_bytes: encoded.len() as u64,
            record_count: records.len() as u64,
            min_start_ns,
            max_start_ns,
            pricing_fingerprint: config.pricing.fingerprint(),
        };
        let rollup = analytics::SegmentRollup::build(&spans, &config.pricing);
        let _ = rollup_file::store(path, binding, &rollup);
    }
    Ok(())
}

/// Writes `bytes` over `path` by the same discipline every writer in this
/// crate uses: a writer-unique dot-prefixed temp in the same directory,
/// fsynced, then renamed onto the SAME final name. The rename is the atom
/// resumability rests on — a crash leaves either the old file or the new one,
/// never a blend, and the orphan-temp sweep (or the next pass) clears the
/// staging file.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let directory = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let counter = crate::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(".{file_name}.{}.{counter}.tmp", std::process::id()));
    let staged = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    // Wrapped with the target path, like every other refusal in this module:
    // a bare "No space left on device" out of `Store::open` names neither
    // the file the migration starved on nor the fact that reopening after
    // freeing space resumes from exactly that file.
    if let Err(error) = staged {
        let _ = fs::remove_file(&temp);
        return Err(refuse(format!(
            "cannot migrate {}: staging its rewrite failed: {error}. The \
             original file is untouched; clear the cause (disk space, above \
             all) and reopen — the migration resumes from this exact file",
            path.display()
        )));
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(refuse(format!(
            "cannot migrate {}: renaming its staged rewrite into place \
             failed: {error}. The original file is untouched; the migration \
             resumes from this exact file on the next open",
            path.display()
        )));
    }
    Ok(())
}

/// The blob pass over one `payloads/` tree. Returns the paths (with reasons)
/// of every file that matched NEITHER format; the caller refuses the whole
/// migration naming them, so the report covers all of them in one pass.
///
/// The three-way rule, never magic alone: a file that passes full v7
/// validation — magic, CRC, decode, SHA-256 of the decoded bytes against the
/// name — is already migrated and left byte-for-byte alone. A file that fails
/// that but whose RAW bytes SHA-256 to the name is a v6 blob (a v6 blob whose
/// content merely begins with the magic lands here, which is why magic alone
/// was never the test) and is rewritten as `TRZBLOB1` onto the same name.
/// Anything else is corrupt and deliberately NOT rewritten.
///
/// Files that are not content-addressed blobs at all (a name that is not 64
/// hex digits plus `.bin`) are outside the payload namespace — the serving
/// path can never read them — and are left untouched rather than classified.
/// Dot-prefixed `.tmp` files are this migrator's own staging leftovers and
/// are removed.
fn migrate_blob_tree(root: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut shards = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(problems),
        Err(error) => return Err(Error::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            shards.push(entry.path());
        }
    }
    shards.sort();
    for shard in shards {
        let mut files = Vec::new();
        for entry in fs::read_dir(&shard)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
        files.sort();
        let mut touched = false;
        for path in files {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = fs::remove_file(&path);
                continue;
            }
            let Some(stem) = name.strip_suffix(".bin") else {
                continue;
            };
            if stem.len() != 64 || !stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let bytes = fs::read(&path)?;
            if let Ok(decoded) = payload::decode_blob(&bytes, stem) {
                if payload::sha256_hex(&decoded).eq_ignore_ascii_case(stem) {
                    continue; // already a valid v7 blob
                }
            }
            if payload::sha256_hex(&bytes).eq_ignore_ascii_case(stem) {
                write_replacing(&path, &payload::encode_blob(&bytes))?;
                touched = true;
            } else {
                problems.push(format!(
                    "{}: not a valid v7 blob, and the raw bytes do not hash to \
                     the file name",
                    path.display()
                ));
            }
        }
        if touched {
            sync_directory(&shard)?;
        }
    }
    Ok(problems)
}

/// Whether a manifested path names a content-addressed payload blob — the
/// shape (`payloads/…/<64-hex>.bin`) the blob pass fully validates or
/// refuses. Everything else under `payloads/` (a crash-stranded ingest temp,
/// a foreign file) is outside the blob pass, so a moved digest on one has no
/// format to prove itself in and must not be re-accepted.
fn is_validated_blob_path(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix(payload::PAYLOAD_DIR) else {
        return false;
    };
    rest.starts_with('/')
        && rest
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".bin"))
            .is_some_and(|stem| {
                stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

/// Migrates one pin: the same two passes as the live store, then the pin's
/// `state-manifest.json` digests rewritten to match — restore verifies a pin
/// against those digests before installing it, and the file passes have just
/// changed every rewritten file's bytes.
///
/// This also IS the resume re-validation: it runs on every migration resume,
/// re-hashes every immutable file the manifest lists, and redoes the manifest
/// rewrite exactly when the files read as v7 but the digests disagree — the
/// crash window between a pin's file pass and its manifest rewrite, which the
/// version-word trigger alone cannot see. Re-validating a finished pin costs
/// a hash pass and changes nothing. The append-only log copies are never
/// touched by migration, so their digests are carried, not recomputed: a
/// damaged log copy stays a verification failure instead of being laundered
/// into a fresh digest.
fn migrate_pin(pin: &Path, config: &Config) -> Result<()> {
    // Staging leftovers from a killed earlier attempt.
    for entry in fs::read_dir(pin)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && name.starts_with('.') && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
    for path in segment_files(pin)? {
        match segment_version(&path)? {
            Some(v6::VERSION) => migrate_segment(&path, config, false)?,
            Some(version) if version == segment::VERSION => {}
            other => {
                return Err(refuse(format!(
                    "cannot migrate pin {}: {} does not read as a v6 or v7 \
                     segment (version {other:?}). Release the pin or restore \
                     the file from the backup that was copied off it, then \
                     reopen",
                    pin.display(),
                    path.display()
                )))
            }
        }
    }
    sync_directory(pin)?;
    let problems = migrate_blob_tree(&pin.join(payload::PAYLOAD_DIR))?;
    if !problems.is_empty() {
        return Err(refuse(format!(
            "migration refuses to rewrite {} payload file(s) in pin {} that \
             match neither format:\n{}",
            problems.len(),
            pin.display(),
            problems.join("\n")
        )));
    }

    let manifest_path = pin.join(generation::MANIFEST_NAME);
    let mut manifest = generation::load_manifest(&manifest_path)?;
    let mut dirty = manifest.segment_format != Some(segment::VERSION);
    for file in &mut manifest.files {
        if generation::is_append_only(&file.path) {
            continue;
        }
        let path = pin.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let metadata = fs::metadata(&path).map_err(|error| {
            refuse(format!(
                "cannot migrate pin {}: manifested file {} is unreadable: \
                 {error}. Release the pin or restore the file, then reopen",
                pin.display(),
                file.path
            ))
        })?;
        let bytes = metadata.len();
        let sha256 = payload::sha256_file(&path)?;
        if file.bytes != bytes || file.sha256 != sha256 {
            // The rewrite happens only "where the files validate as v7 but
            // the digests disagree" — validate, not version-word. Blobs were
            // fully validated (or refused) by the pass above; a segment whose
            // digest moved must prove ALL of its bytes before the new digest
            // is accepted, or a bit-flipped pinned segment would be laundered
            // into a manifest that then verifies clean over garbage. Eager
            // (`from_bytes`), not the lazy `Segment::open`: the lazy open
            // reads only the header and index sections and leaves the
            // compression blocks — ~90% of the bytes — CRC-checked at query
            // time, which is exactly the laundering window this check
            // exists to close. The cost is one full read of a digest-moved
            // pinned segment, on the resume path only.
            if file.path.ends_with(crate::SEGMENT_SUFFIX) {
                let validate =
                    fs::read(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| {
                            segment::Segment::from_bytes(bytes)
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        });
                if let Err(error) = validate {
                    return Err(refuse(format!(
                        "cannot migrate pin {}: {} does not validate as a v7 \
                         segment ({error}); its manifest digest is left \
                         alone. Release the pin or restore the file, then \
                         reopen",
                        pin.display(),
                        path.display()
                    )));
                }
            } else if !is_validated_blob_path(&file.path) {
                // Neither a segment nor a content-addressed blob: nothing
                // this migrator rewrites, and nothing it can validate — a
                // crash-stranded ingest temp under `payloads/`, above all.
                // Its digest moving means the pinned bytes changed OUTSIDE
                // migration, and accepting the new digest would launder that
                // damage into a manifest that then verifies clean over it.
                return Err(refuse(format!(
                    "cannot migrate pin {}: manifested file {} changed on \
                     disk, and it is not a file this migrator can validate \
                     (not a segment, not a content-addressed blob), so its \
                     manifest digest is left alone. Release the pin or \
                     restore the file, then reopen",
                    pin.display(),
                    file.path
                )));
            }
            file.bytes = bytes;
            file.sha256 = sha256;
            file.modified_unix_ns = metadata
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
            dirty = true;
        }
    }
    if dirty {
        manifest.segment_format = Some(segment::VERSION);
        generation::write_pin_manifest(pin, &manifest)?;
    }
    Ok(())
}

/// The completion checkpoint. Its manifest declares the store format, and
/// every later checkpoint carries the declaration forward.
///
/// **A full re-hash, forbidden from carrying any digest forward** — the empty
/// `prior` slice is that prohibition made structural, not an optimization
/// left on the table. The incremental rule lets an ordinary checkpoint carry
/// a digest over because segments are immutable; migration has just violated
/// that premise wholesale, and a carried digest would publish a generation
/// that fails verification everywhere — verify-at-pin fails, restore is
/// impossible.
///
/// `folded_through` is published UNCHANGED from the prior generation: the
/// migrator folds nothing, because it does not replay the WAL. Frames after
/// the fold replay normally against the migrated store as soon as this
/// returns.
fn complete(
    directory: &Path,
    live_generation: u64,
    folded_through: generation::FoldedThrough,
) -> Result<u64> {
    let files = generation::digest_engine(directory, &[])?;
    let next = live_generation + 1;
    let created_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos().min(u128::from(u64::MAX)) as u64);
    generation::write_manifest(
        directory,
        &generation::Manifest {
            generation: next,
            created_unix_ns,
            folded_through,
            files,
            segment_format: Some(segment::VERSION),
        },
    )?;
    generation::publish_current(directory, next)?;
    // Housekeeping, exactly as after any checkpoint: a failure here merely
    // postpones the sweep.
    let _ = generation::sweep_generations(directory, next);
    Ok(next)
}

/// The FROZEN v6 decoder — records only, copied from `src/segment.rs` as of
/// commit `5f23172` (the last commit whose live reader spoke v6) and then cut
/// down to what migration needs. It exists nowhere else: the serving path
/// reads exactly one format, and this module is the one place the old layout
/// is still understood.
///
/// What was kept: the v6 header parse with its full section-contiguity and
/// trailing-byte validation, the record-offset decode with its ordering
/// checks, and the record decode (length-prefixed attribute strings walked
/// and UTF-8-validated, then discarded — the migrator re-derives every
/// attribute from the payload, which is the format's own definition of the
/// pair list). What was dropped: the index decoders, the query paths, and the
/// encoder — migration rebuilds all of that through the live v7 encoder.
mod v6 {
    /// The one version this decoder reads. Everything else was refused by the
    /// v6 reader too, and is refused here with the same certainty.
    pub(super) const VERSION: u16 = 6;
    const MAGIC: [u8; 8] = *b"TRAZASEG";
    const HEADER_LEN: usize = 104;
    const RECORD_FIXED_LEN: usize = 8 + 4 + 4 + 4 + 4;

    /// One decoded v6 record. The attribute strings were walked for
    /// structural validation but are not carried: the payload is the
    /// authority the migrator re-derives from.
    pub(super) struct Record {
        pub timestamp: u64,
        pub trace_id: String,
        pub payload: Vec<u8>,
    }

    struct Header {
        record_count: u64,
        records_offset: u64,
        records_len: u64,
        offsets_offset: u64,
        offsets_len: u64,
        trace_index_offset: u64,
        trace_index_len: u64,
        attribute_index_offset: u64,
        attribute_index_len: u64,
        content: (u64, u64),
    }

    /// Decodes a complete v6 segment file into its records, validating the
    /// header, the section layout, and every record's framing exactly as the
    /// frozen reader did. Errors are reasons; the caller names the file.
    pub(super) fn decode_segment(bytes: &[u8]) -> Result<Vec<Record>, String> {
        let header = parse_header(bytes)?;
        let offsets = decode_offsets(bytes, &header)?;
        let mut records = Vec::with_capacity(offsets.len());
        for offset in &offsets {
            records.push(decode_record(
                bytes,
                (header.records_offset + *offset) as usize,
                (header.records_offset + header.records_len) as usize,
            )?);
        }
        Ok(records)
    }

    fn parse_header(bytes: &[u8]) -> Result<Header, String> {
        if bytes.len() < HEADER_LEN {
            return Err("file is shorter than the v6 header".to_owned());
        }
        if bytes[..8] != MAGIC {
            return Err("not a Traza segment (bad magic)".to_owned());
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != VERSION {
            return Err(format!("declares format v{version}, not v6"));
        }
        let header_len = u16::from_le_bytes([bytes[10], bytes[11]]);
        if usize::from(header_len) != HEADER_LEN {
            return Err("header length does not match the version".to_owned());
        }
        let total = bytes.len() as u64;
        let attribute_index_offset = read_u64(bytes, 72)?;
        let content_offset = read_u64(bytes, 96)?;
        let header = Header {
            record_count: read_u64(bytes, 16)?,
            records_offset: read_u64(bytes, 24)?,
            records_len: read_u64(bytes, 32)?,
            offsets_offset: read_u64(bytes, 40)?,
            offsets_len: read_u64(bytes, 48)?,
            trace_index_offset: read_u64(bytes, 56)?,
            trace_index_len: read_u64(bytes, 64)?,
            attribute_index_offset,
            attribute_index_len: content_offset
                .checked_sub(attribute_index_offset)
                .ok_or("attribute index offset beyond file")?,
            content: (
                content_offset,
                total
                    .checked_sub(content_offset)
                    .ok_or("content index offset beyond file")?,
            ),
        };
        // The v6 contiguity rule: five sections, each starting where the
        // previous ends, the last ending at EOF, trailing bytes refused.
        let sections = [
            (header.records_offset, header.records_len),
            (header.offsets_offset, header.offsets_len),
            (header.trace_index_offset, header.trace_index_len),
            (header.attribute_index_offset, header.attribute_index_len),
            header.content,
        ];
        let mut expected = HEADER_LEN as u64;
        for (offset, len) in sections {
            if offset != expected {
                return Err("sections are not contiguous".to_owned());
            }
            expected = offset.checked_add(len).ok_or("section bounds overflow")?;
            if expected > total {
                return Err("section exceeds file bounds".to_owned());
            }
        }
        if expected != total {
            return Err("trailing or unaccounted segment bytes".to_owned());
        }
        let expected_offsets = header
            .record_count
            .checked_mul(8)
            .ok_or("record-offset index length overflow")?;
        if header.offsets_len != expected_offsets {
            return Err("record-offset index has invalid length".to_owned());
        }
        Ok(header)
    }

    fn decode_offsets(bytes: &[u8], header: &Header) -> Result<Vec<u64>, String> {
        let start = header.offsets_offset as usize;
        let end = start + header.offsets_len as usize;
        let data = bytes
            .get(start..end)
            .ok_or("offsets section out of bounds")?;
        let mut offsets = Vec::with_capacity(header.record_count as usize);
        let mut previous = None;
        for chunk in data.chunks_exact(8) {
            let offset = u64::from_le_bytes(chunk.try_into().expect("eight-byte chunk"));
            if offset >= header.records_len || previous.is_some_and(|value| offset <= value) {
                return Err("record offsets are invalid or unordered".to_owned());
            }
            previous = Some(offset);
            offsets.push(offset);
        }
        if offsets.is_empty() && header.records_len != 0 {
            return Err("record region exists without records".to_owned());
        }
        Ok(offsets)
    }

    fn decode_record(bytes: &[u8], start: usize, region_end: usize) -> Result<Record, String> {
        if start
            .checked_add(RECORD_FIXED_LEN)
            .filter(|end| *end <= region_end)
            .is_none()
        {
            return Err("truncated record header".to_owned());
        }
        let timestamp = read_u64(bytes, start)?;
        let trace_len = read_u32(bytes, start + 8)? as usize;
        let attribute_count = read_u32(bytes, start + 12)? as usize;
        let payload_len = read_u32(bytes, start + 16)? as usize;
        let mut cursor = start + RECORD_FIXED_LEN;
        let trace = take(bytes, &mut cursor, trace_len, region_end)?;
        let trace_id = std::str::from_utf8(trace)
            .map_err(|_| "trace id is not valid UTF-8".to_owned())?
            .to_owned();
        for _ in 0..attribute_count {
            // Walked and validated, then discarded: v6 stored each pair as
            // two length-prefixed strings, and the migrator re-derives the
            // pairs from the payload instead of trusting these.
            let key = take_len_bytes(bytes, &mut cursor, region_end)?;
            let value = take_len_bytes(bytes, &mut cursor, region_end)?;
            std::str::from_utf8(key).map_err(|_| "attribute key is not valid UTF-8".to_owned())?;
            std::str::from_utf8(value)
                .map_err(|_| "attribute value is not valid UTF-8".to_owned())?;
        }
        let payload = take(bytes, &mut cursor, payload_len, region_end)?.to_vec();
        Ok(Record {
            timestamp,
            trace_id,
            payload,
        })
    }

    fn take_len_bytes<'a>(
        bytes: &'a [u8],
        cursor: &mut usize,
        end: usize,
    ) -> Result<&'a [u8], String> {
        let len = {
            let next = cursor.checked_add(4).ok_or("field length overflow")?;
            if next > end || next > bytes.len() {
                return Err("truncated length prefix".to_owned());
            }
            let raw = &bytes[*cursor..next];
            *cursor = next;
            u32::from_le_bytes(raw.try_into().expect("four-byte slice")) as usize
        };
        take(bytes, cursor, len, end)
    }

    fn take<'a>(
        bytes: &'a [u8],
        cursor: &mut usize,
        len: usize,
        end: usize,
    ) -> Result<&'a [u8], String> {
        let next = cursor.checked_add(len).ok_or("field length overflow")?;
        if next > end || next > bytes.len() {
            return Err("truncated variable-length field".to_owned());
        }
        let value = &bytes[*cursor..next];
        *cursor = next;
        Ok(value)
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
        let raw = bytes.get(offset..offset + 4).ok_or("truncated integer")?;
        Ok(u32::from_le_bytes(raw.try_into().expect("four-byte slice")))
    }

    fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
        let raw = bytes.get(offset..offset + 8).ok_or("truncated integer")?;
        Ok(u64::from_le_bytes(
            raw.try_into().expect("eight-byte slice"),
        ))
    }
}
