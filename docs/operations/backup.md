# Backup and restore

Backup is **pin, verify, copy** — with the server running. Restore is
**install**. Both are the same mechanism seen from two ends: a *generation*,
which is one immutable, self-describing state of the store.

## What a generation is

Query-visible state used to live in several independent recovery domains — the
write-ahead log and write buffer, segments, `annotations.jsonl`, `payloads/` —
each with its own durability rule and its own idea of "now". Nothing named a
state they all agreed on, which is why backup, export, retention and deletion
were four mechanisms instead of one.

A generation is that agreed state: a manifest listing every load-bearing file
with its SHA-256 digest, plus the log position (`folded_through`) at which the
manifest's contents end and replay begins. `CURRENT` names the live one, and
moving it — a staged rename made durable by a directory fsync — is the single
commit point for a checkpoint, a restore, or a published deletion.

```text
data/
  LOCK
  CURRENT                    names the live generation
  wal.log                    frames stamped with the generation they belong to
  segment-*.seg              the working set: segments,
  annotations.jsonl          the annotation log,
  payloads/                  and offloaded payload bytes
  generations/<id>/state-manifest.json
  pins/<label>/              a pin: hard links to one manifest's files
```

Existing data directories are adopted at first open — nothing moves, and
generation one is published over the files already there. It is one-way: an
adopted directory is not readable by a pre-generation binary.

## Taking a backup

Against a running server, with ingest continuing throughout:

```sh
curl -X POST http://localhost:8080/v1/backups/nightly
```

```json
{"backup":"nightly","generation":42,
 "path":"/var/lib/traza/pins/nightly","verified":true}
```

That checkpoints, hard-links the manifested files into `pins/nightly`, and
verifies every digest before reporting success. Hard links share inodes, so the
pin costs almost no disk **and holds its bytes even after compaction unlinks
the originals** — which is what lets the copy proceed at its own pace:

```sh
cp -a /var/lib/traza/pins/nightly /backups/traza-$(date +%F)
curl -X POST http://localhost:8080/v1/backups/nightly/release
```

Release it when the copy is done. A pin left in place keeps its generation's
bytes on disk, so an unreleased pin is a slow disk leak, not a correctness
problem.

The copy carries **spans, annotations, and payload bytes together**. That is
the part a span export never could: an export pinned spans and nothing else, so
a dataset export meant only part of a session's state.

## Verifying

```sh
curl http://localhost:8080/v1/verify
```

```json
{"generation":42,"intact":true,"problems":[]}
```

Every file in the live generation's manifest is re-read and re-digested.
Problems are reported by name — `segment-…seg: digest mismatch`,
`payloads/ab/cd.bin: missing` — rather than as one boolean, because knowing
*which* file is damaged is what decides whether to restore.

This is what lets recovery distinguish "damage I may safely ignore" from
"damage that changes what the store contains" by asking rather than inferring
it from whether parsing happened to succeed.

## Restoring

Restore is offline: it replaces the working set wholesale, which a live store
cannot tolerate. Point a server at the backup with `--restore`:

```sh
traza-server --data-dir /var/lib/traza --restore /backups/traza-2026-08-10
```

The backup is verified **before** anything is swapped, and the swap commits at
one `CURRENT` rename, so a failed or interrupted restore leaves the prior store
rather than a blend. The server then serves the restored generation normally.
The library equivalent is [`Store::restore`](../../src/lib.rs).

A restored store starts with an empty write-ahead log: its state is entirely in
the installed files, and any prior log belonged to a different lineage.

## Checkpoints

A checkpoint publishes a generation. The server takes one every five minutes,
`pin` takes one implicitly, and you can ask for one:

```sh
curl -X POST http://localhost:8080/v1/checkpoint
```

Nothing depends on the cadence for correctness — recovery excludes folded
frames by their stamp whether or not a checkpoint has run recently — but each
one moves `folded_through` forward, which bounds what a restart replays.

Checkpoints are cheap by construction: segments are immutable, so a checkpoint
carries their digests over from the previous manifest and hashes only what was
written since.

**A checkpoint is never a side effect of a primitive.** Expiry deletes from
every domain it touches and stops there; it does not seal your write buffer to
publish a manifest. The deletion is durable when its domains are durable, and
*published* by the next checkpoint — within one maintenance interval, or
immediately when a backup asks.

## What a deletion means here

Expiring a span removes it from the buffer, rewrites the log so a restart
cannot replay it, rewrites or retires the segments holding it, and sweeps the
annotations and payload files that only it referenced. The next checkpoint then
publishes a generation whose manifest no longer names those bytes.

The frames a checkpoint folds are excluded from replay **by their stamp**, not
by having been physically removed. That is what makes reclaiming them
housekeeping rather than correctness: a crash between the `CURRENT` rename and
the log roll-over leaves a log still holding frames the new generation
contains, and recovery discards them because they are at or before
`folded_through`. Without that rule, a crash in that window would replay a
deletion back into existence.

## What is not covered

- **Off-host copying is yours.** Traza will not write outside its own data
  directory, so `pins/<label>` is a path to copy, not a destination to
  configure.
- **A pin is not an archive.** It shares inodes with the live store, so it
  survives compaction but not the disk.
- **Restore is whole-store.** There is no partial or point-in-time restore
  within a generation; the granularity is the generation.

## See also

- [Durability](durability.md) — what an acknowledgement means per mode
- [Deployment](deployment.md) — one writer per directory, and what lives on disk
- [Invariants](../internals/invariants.md) — rule 12 states the boundary
