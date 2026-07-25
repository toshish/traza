# Durability

An acknowledged write means what `--durability` says it means — no more, and no
less. The mode is chosen per deployment, announced at startup, echoed in every
ingest response, and reported by `GET /v1/stats`, so a client never has to
guess.

## The three modes

Ordering of strength: `buffered` < `wal` < `flushed`.

| Mode | A `200` means | Cost |
|---|---|---|
| `buffered` | The batch is accepted **in memory**. A crash loses anything not yet sealed into a segment | Fastest, and **lossy by design** |
| `wal` (default) | The batch is fsynced to the write-ahead log and will be recovered on restart | One group-committed fsync per batch |
| `flushed` | The batch is present in a sealed segment | A segment write per ingest call |

```sh
traza-server --data-dir ./data --durability wal
```

Every ingest response names the mode:

```json
{"accepted":1,"durability":"wal"}
```

### `buffered`

No durability is promised at all. The store neither writes nor reads a log, and
acknowledged spans live in memory until a flush seals them. A crash — process
kill, panic, OS crash, power cut — loses everything not yet sealed.

This is the right choice for laptops, CI, and benchmarks, and the wrong choice
for anything whose loss you would notice. The server says so on startup:

```
traza-server: durability=buffered — acknowledged writes are IN MEMORY ONLY and a crash loses anything not yet flushed. Use --durability wal in production.
```

An existing `wal.log` is left untouched in this mode, so restarting the same
directory in `wal` mode still recovers it.

### `wal` — the default

A batch is appended to `wal.log` and **fsynced before the response is sent**.
On open, the log is replayed into the write buffer before any new write is
accepted. Once a flush seals those spans into a segment the log is superseded
and truncated.

This is the production default because a store that silently loses
acknowledged writes is the wrong default, even though it is the faster one.

**Group commit** is what makes it affordable. The fsync runs *outside* the
writer lock: concurrent batches keep appending while one thread is syncing, and
a single fsync then covers all of them. A waiter wakes when some sync has
covered its log sequence number. Under concurrency, per-batch cost therefore
does not become per-batch fsync.

`--wal-commit-window-us` deliberately holds an fsync open a little longer so
more batches join it. It buys more amortization at the cost of delaying every
acknowledgement in the window by up to that long. It helps when batches arrive
steadily but concurrency is too low to fill a sync on its own, and it hurts an
idle store, which is why it is off by default. **It never weakens the
guarantee** — the acknowledgement still follows the fsync.

### `flushed`

Every ingest call seals a segment before acknowledging, so a `200` means the
data is in an immutable, fsynced, atomically renamed file. The strongest and
by a wide margin the slowest mode.

## What survives what

| Failure | `buffered` | `wal` | `flushed` |
|---|---|---|---|
| `kill -9` / SIGKILL | Loses unsealed writes | **Survives** | **Survives** |
| Process panic | Loses unsealed writes | **Survives** | **Survives** |
| OS crash | Loses unsealed writes | **Survives** | **Survives** |
| Power cut, Linux | Loses unsealed writes | **Survives** | **Survives** |
| Power cut, macOS | Loses unsealed writes | *See below* | *See below* |

`tests/durability.rs` is the oracle for the first three rows. It uses SIGKILL —
no unwinding, no destructors, no flush on the way out — and holds each mode to
exactly what it claims, including verifying that `buffered` is genuinely lossy
rather than accidentally durable.

## The macOS caveat, stated plainly

**On macOS, `fsync` does not flush the drive's own write cache.**

Traza's `wal` and `flushed` modes call `File::sync_data`, which is `fsync(2)`.
On Linux that carries the usual guarantee. On macOS, flushing the device cache
requires `F_FULLFSYNC`, which the Rust standard library does not expose and
which this crate will not reach for while it forbids unsafe code and carries
two dependencies.

The consequence, precisely:

- **A macOS machine losing power can still lose an acknowledged write**, in
  `wal` and in `flushed`, even though the fsync returned successfully.
- **A `kill -9`, a panic, or an OS crash cannot** — on either platform. The
  data has left the process and reached the kernel, which is what those
  failures do not disturb.
- **On Linux, `fsync` carries the usual guarantee** and there is no such gap.

This is documented at source in [`src/wal.rs`](../../src/wal.rs). If you are
deploying on macOS and power loss is in your threat model, that gap is real and
Traza does not currently close it.

## Recovery

Recovery is **ordered**, and it follows journals rather than content.

- **Log records replay in append order** and are upserted in that order, so a
  re-ingested span recovers as its newest version — exactly as last-write-wins
  had it before the crash.
- **A torn or corrupt trailing record is discarded.** Frames carry a length and
  a CRC32; replay stops at the first short or corrupt frame and keeps
  everything before it. Trailing garbage is never interpreted. Discarding is
  correct: that record had not been acknowledged, because the acknowledgement
  follows the fsync.
- **Segments appear atomically.** Write to a temp file, fsync the file, rename
  into place, fsync the directory. A reader sees a complete segment or no
  segment.
- **Interrupted compaction rewrites are finished from the supersede journal.**
  A marker is written and fsynced before the replacement exists. At open, if
  the replacement exists and parses, the original is deleted; if it never
  materialized, nothing is deleted and the merge is simply retried. Recovery
  never inspects content to decide — an earlier content-based approach silently
  destroyed legitimately re-ingested identical spans.
- **Orphaned temp files** from an interrupted segment write are swept at open.
- **Annotations and payloads fsync on their own path.** A torn trailing
  annotation line is ignored, matching the segment layer.
- **A stale directory lock is reclaimed.** A lock naming a PID that verifiably
  no longer exists does not wedge the store; a live owner still rejects the
  open.

## Choosing a mode

- **Production: `wal`.** It is the default for a reason. The fsync is
  amortized, and an acknowledged write survives everything except the macOS
  power-cut gap above.
- **`flushed`** only when you need a `200` to mean "in an immutable file" —
  for example a low-volume audit trail. Expect a segment write per call.
- **`buffered`** for laptops, CI, benchmarks, and bulk loads you can redo. The
  `seed` tool uses it for exactly that reason: it is a bulk load into a fresh
  store that is flushed at the end anyway.

You can also lower `--flush-spans` to shorten the window in `buffered` mode,
but that is a mitigation, not a guarantee. If you need a guarantee, use `wal`.

## Backups

Segments are immutable and the write-ahead log is small, but a data directory
is still a live, self-consistent set of files. The safe procedures:

- **Cold copy.** Stop the server, copy the whole directory, restart.
  Unambiguously correct.
- **Filesystem or volume snapshot.** A snapshot that is atomic across the whole
  directory gives a crash-consistent image, which is exactly what Traza's
  recovery is built to handle: it is the same state a `kill -9` would leave.

Copy the **entire** directory — segments, `wal.log`, `annotations.jsonl`, and
`payloads/` together. A span whose oversized attribute was offloaded is
incomplete without its payload file, and a segment set without its log is
missing every acknowledged write not yet sealed.

Do **not** rsync a running directory file by file: an in-flight flush can
change the segment set between files, and a partial `.tmp` or
`.supersede.*.journal` copied without its counterpart is meaningless. If you
must copy hot without a snapshot, `POST /v1/flush` first to seal the buffer and
truncate the log — it narrows the window but does not eliminate it.

Restore is a directory copy into place with **no server running against it**.
Remember the single-writer rule: never point two processes at one directory,
including a backup agent that writes.

## See also

- [Configuration reference](../configuration.md) — `--durability`,
  `--flush-spans`, `--wal-commit-window-us`
- [Monitoring](monitoring.md) — `wal_bytes`, fsync timings, group-commit ratio
- [Invariants](../internals/invariants.md) — the ordering rules this contract
  rests on
