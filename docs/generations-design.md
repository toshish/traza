# Single-node generations and checkpoints

## Status

**Design, not shipped behaviour.** This proposes bringing the generation and
checkpoint model of the [HA design](ha-design.md) forward into the single-node
engine, *before* replication rather than as part of it. Nothing here exists in
the code today. It is written against v0.18 and is a prerequisite proposal for
Phase 1, not a description of what a current server does.

## Why this document exists

An independent review of v0.17 found four defects. Each was fixed on its own
terms and each fix is tested (see the [changelog](../CHANGELOG.md)):

| Defect | Immediate fix |
|---|---|
| TTL removed expired spans from memory but not from the write-ahead log, so a restart brought them back | Expiry rewrites the log to the survivors |
| Export paged the live store, so a concurrently re-ingested span appeared twice under a `complete: true` trailer | Export pins a `SnapshotView` and pages that |
| Log recovery stopped at the first bad frame and reported success, silently discarding acknowledged batches after it | Interior damage refuses to open; only a torn tail is dropped |
| The flush threshold counted unique buffered records, so hot-key updates grew the log without bound | The threshold counts upserts and log bytes too |

The reviewer's more important observation was that these are **four symptoms of
one missing abstraction**. Traza has several independent recovery domains:

```
write-ahead log + write buffer
segments
annotations.jsonl
payloads/
retention decisions
export pagination
```

Each has its own notion of what is durable, its own recovery rule at open, and
its own idea of what "now" means. Nothing names a state that all of them agree
on. Read the four defects again in that light:

- **Retention** changed one domain (memory) and not the authority (the log).
- **Export** read a state that no single domain was ever in, because it sampled
  repeatedly over time.
- **Recovery** could not tell "damage I may safely ignore" from "damage that
  changes what the store contains", because nothing declares how much of the
  log is supposed to be there.
- **Flush policy** used a memory-shaped number (unique buffered keys) to bound
  a recovery-shaped cost (bytes to replay).

The fixes are correct and they are not a substitute for the abstraction. Each
one adds a rule that a future change must remember: rewrite the log when you
delete, pin a view when you page, count bytes as well as records. A generation
boundary makes those rules structural instead of remembered.

## What a generation is

One immutable, self-describing, complete logical state of the store,
identified by a monotonically increasing generation id and a manifest that
names every file in it and a digest for each.

The layout is the one the [HA design](ha-design.md#node-layout-and-ownership)
already specifies, minus everything consensus-related:

```text
data/
  LOCK
  CURRENT                      -- names the live generation
  generations/
    <generation-id>/
      engine/
        segment-*.seg
        annotations.jsonl
        payloads/
      state-manifest.json      -- files, digests, applied sequence, created-at
  wal.log                      -- the delta on top of CURRENT
  incoming/                    -- staging for an install
```

Two rules give the model its value:

1. **A generation is immutable once published.** It is replaced, never edited.
2. **`CURRENT` is published by one atomic rename**, and it is the only thing
   that decides which generation a restart loads.

## The four operations

**Pin.** Take a reference to the live generation plus the write buffer as it
stands. A pinned generation cannot be reclaimed while a reader holds it. This
is what `SnapshotView` does today for spans; the generation form extends it to
annotations and payload bytes, which a span export currently cannot pin at all.

**Checkpoint.** Seal the write buffer, fold the log into the generation, write
a new manifest naming an applied sequence number, fsync, and publish by
renaming `CURRENT`. After a checkpoint the log is empty by construction rather
than by a separate reclamation step.

**Verify.** Re-read a manifest and check every digest. A store can then answer
"is this generation intact" without inferring it from whether parsing happened
to succeed. This is what makes recovery able to distinguish the two kinds of
damage the log-recovery fix currently distinguishes by frame structure alone.

**Install.** Stage a complete generation under `incoming/`, verify it, and swap
`CURRENT`. A partially transferred generation is never the live one, because
the swap is one rename of a file whose contents are a single name.

## What it collapses into one mechanism

- **Backup** becomes: pin, verify, copy the generation. No stop-the-server, no
  "copy the whole directory and hope the flush did not land mid-copy", no rsync
  caveat.
- **Restore** becomes: install.
- **Export** becomes: pin, then read — the same pin, extended past spans to
  annotations and payloads, so a dataset export finally means all of a
  session's state rather than only its spans.
- **Retention** becomes: apply the deletion to a new generation and publish it.
  A deletion is durable when `CURRENT` moves, which is one fact to test rather
  than one per domain. The compliance question ("is it actually gone?") gets a
  single answer.
- **Replication** becomes: ship generations plus the log delta between them —
  which is exactly what the HA design's snapshot transfer needs. Phase 2 stops
  having to invent it.
- **Recovery** becomes: load `CURRENT`, replay the log delta, and have one
  place that says how much of the log belongs to this generation.

## Cost, honestly

- **A checkpoint rewrites the manifest, not the corpus.** Segments are already
  immutable and are hard-linked or referenced across generations; only the
  manifest and newly sealed files are written. A generation is not a copy of
  the store.
- **Reclamation gets slower to reason about.** A pinned generation holds disk
  until it drops, exactly as a pinned `SnapshotView` does today. A parked
  export or a stalled backup delays reclamation, and that needs a bound and a
  metric, not just documentation.
- **The directory layout changes**, which is a migration: an existing data
  directory becomes generation zero at first open. This is a one-way migration
  and must ship before the format freeze, per the roadmap's "identity before
  features" principle.
- **It is a substantial change to `src/lib.rs`**, the file the
  [invariants](internals/invariants.md) live in. Several of those invariants
  (1, 5, 6, 10) become properties of the generation boundary instead of rules
  each operation has to observe separately — which is the point, and also the
  risk.

## Sequencing

This belongs in **Phase 1**, before v1.0 and before HA:

1. Manifests and `CURRENT`, with existing directories migrating to generation
   zero. No behaviour change beyond the layout.
2. `pin` extended from segments to a whole generation; export and backup move
   onto it.
3. `checkpoint`, replacing ad-hoc log reclamation; the flush policy becomes a
   checkpoint policy with the same bounds it has now (records, upserts, log
   bytes).
4. `verify` and `install`, which give backup/restore an end-to-end digest check
   and give Phase 2 its snapshot transfer for free.
5. Retention re-expressed as "publish a generation without the expired spans",
   retiring the per-domain deletion paths.

Phase 2's HA work then adds consensus *on top of* an engine that already has
the boundary it needs, rather than introducing both at once.

## See also

- [High-availability design](ha-design.md) — where this model comes from, and
  the consensus layer it is a prerequisite for.
- [Invariants](internals/invariants.md) — the rules this would subsume.
- [Roadmap](roadmap.md) — phase placement.
