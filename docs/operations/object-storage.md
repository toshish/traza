# Object-storage snapshot archive (preview)

The archive publishes a **verified pin** — the same immutable file set
[backup](backup.md) produces — to S3-compatible object storage as one
**explicitly named, immutable snapshot**, and reads it back three ways:

- **queryable in place**: trace lookups, filtered span queries, cursor
  pagination, and payload retrieval run against the remote by *ranged
  reads*, without downloading segments;
- **restorable**: a full, verified restore into a fresh local directory
  that `traza-server --restore` installs;
- **manageable**: list, inspect, verify, delete, and clean up snapshots.

It ships behind the `object-storage` cargo feature (off by default) and the
`traza-object` binary:

```sh
cargo build --release --features object-storage --bin traza-object
```

The standalone engine keeps its MSRV (Rust 1.81). This feature's dependency
graph (`object_store` and its HTTP/TLS stack) needs a newer toolchain —
currently Rust 1.89, recorded in `Cargo.toml` under
`package.metadata.traza.object-storage-rust-version`.

## What the archive is — and is not

An archive snapshot is **historical, immutable state**, taken explicitly.
The live writer stays local and unchanged. This is *not* tiering: nothing
moves automatically, no write is acknowledged remotely, and there is no
failover. What it earns you is an archive you can query without holding a
local copy — **the disk saved is the local archive copy's, not the live
store's**. Publishing a snapshot does not shrink the data directory.

Snapshots are independent copies with their own lifecycle:

- **Local retention does not reach archives.** TTL expiry and erasures
  operate on the live store; a snapshot published *before* a deletion still
  holds the deleted data. Retiring archived data means **deleting the
  snapshots that hold it** (`traza-object delete`), and your compliance
  process must treat published snapshot ids as part of the data inventory.
- **Deleted snapshot ids are permanently retired.** `delete` writes a
  small `TOMBSTONE` object first and never removes it: readers refuse the
  id, publishers refuse to claim or commit it, and an interrupted delete
  resumes from the tombstone. That permanence is the fencing — it is what
  makes a delete safe against a publisher racing the same id from another
  machine, without a distributed lock. Use fresh ids (dates, nonces);
  never plan to reuse one.
- **The bucket can outlive the delete.** S3 versioning keeps deleted
  objects as prior versions, and Object Lock can forbid deletion outright.
  If erasability matters, archive into a bucket without versioning/lock
  (or with lifecycle rules that expire noncurrent versions), and verify
  your bucket configuration — the tool cannot see or override it.
- **Configure `AbortIncompleteMultipartUpload`.** Large files upload
  multipart. The publisher aborts its upload on every error path it
  survives, but after a publisher *crash* the already-uploaded parts are
  invisible to object listing — neither `cleanup` nor `delete` can see
  them, and they bill until the bucket's lifecycle rule reaps them. Set
  the rule (a few days is typical) on any bucket that receives archives.
- **Pending erasures block publication.** A pin whose tombstone log records
  an *unsettled* erasure still contains the subject's bytes (masked
  locally, bare to any other reader), so `publish` refuses it. Let the
  purge settle, then pin again. `Store::pin_for_object_archive` (and the
  `pin` subcommand) enforce the same rule at pin time — and every reader
  independently re-checks the *archived* tombstone log before serving
  queries, so a damaged or foreign manifest cannot smuggle one through.

## Trust model

Stated precisely, including its root:

- Every object is **fully read back from the remote and SHA-256-verified**
  before the manifest that names it is written. An S3 ETag is never treated
  as a checksum.
- The **manifest is written last**, with a conditional create
  (`If-None-Match`), so a snapshot id has exactly one winner and a reader
  never sees a partial snapshot — no manifest, no snapshot. There are no
  mutable names (`LATEST` does not exist; use explicit ids and `list`).
- Every remote **range read is verified** against the manifest's per-chunk
  SHA-256 table — including reads served from the local cache — and a
  remote segment open runs *exactly* the validation a local open runs
  (header CRC, section checksums, timestamp grounding), because both share
  one implementation. Before any query, the manifest is additionally
  **bound to the generation manifest it archived**: the file sets must
  agree exactly, so an omitted or substituted segment fails the open
  rather than shrinking answers.
- **The manifest is the trust root, and it lives in the bucket.** The
  digests above authenticate data *relative to the manifest*; the manifest
  itself is protected by your credentials, TLS, and the bucket's write
  control. A party able to rewrite the manifest AND every object it names
  could rewrite history undetected — this is not Byzantine-proof storage.
  The defense is external retention: `publish` prints `manifest_sha256`;
  keep it outside the bucket and hand it back with
  `--expect-manifest-sha` (or `RemoteOptions::expected_manifest_sha256`),
  and any rewritten manifest is refused.
- **Store identity is checked everywhere.** `--store-id` is stamped into
  manifests, upload markers and deletion tombstones, and every read,
  verify, delete and cleanup refuses an object recorded under a different
  identity — two stores sharing a prefix cannot read or delete each
  other's snapshots through a configured client. An empty client identity
  skips the check; this guard does not replace bucket access controls.
- **Credentials use the S3 client's environment and workload chain**:
  static access keys/session token, web identity, container credentials,
  then IMDS. Shared AWS profiles, SSO profiles, and credential-process
  profiles are not loaded. The tool takes no secret flags. TLS
  verifies by default; `--allow-http` is an explicit opt-in for local test
  endpoints (MinIO and friends).
- An explicit `--endpoint` overrides inherited endpoint settings. With
  `--virtual-hosted`, a custom endpoint must already include the bucket
  hostname; path-style addressing is the default.
- Backends that lack conditional create cannot give the single-winner
  guarantee; AWS S3, MinIO and current S3-compatible services support it.

## Publishing

Take a pin from a **running** server (preferred — no lock contention):

```sh
curl -X POST http://localhost:8080/v1/backups/nightly
traza-object publish --pin /var/lib/traza/pins/nightly \
    --snapshot nightly-2026-09-06 \
    --bucket my-archive --prefix traza --store-id prod-traza
curl -X POST http://localhost:8080/v1/backups/nightly/release
```

Or from a **stopped** store (the subcommand refuses a live one):

```sh
traza-object pin --data-dir /var/lib/traza --label nightly
```

`publish` validates the pin BEFORE opening anything it names — canonical
file naming, no symlinks anywhere, no live-store artifacts (`wal.log`,
`CURRENT`, lock files), and **no unmanifested files**, so a stale manifest
beside newer segments refuses rather than silently archiving the older
subset. It then refuses pending erasures, re-verifies every digest, packs
files into bounded objects (~8 MiB packs; larger files stream multipart as
their own object), reads every object back for verification, and only then
writes the manifest. Failures before the manifest is created leave **no
visible snapshot**. A publish error or timeout can also mean an **unknown
outcome**: the manifest may have committed, or a later check or upload-marker
removal may have failed. Once the publisher has stopped, inspect the same
snapshot id and run `verify --deep` with the same backend, prefix and store
identity before deciding what to clean up or retry. If complete, retain it;
`cleanup` only removes its stale upload marker. A failed inspection does
not establish absence.

If no manifest exists and no publisher is running anywhere, remove the
abandoned upload with:

```sh
traza-object cleanup --snapshot nightly-2026-09-06 --publisher-quiescent \
    --bucket my-archive --prefix traza
```

`--publisher-quiescent` is your explicit statement that **no publisher for
this snapshot id is running anywhere**; without it, sweeping a manifest-less
upload is refused, because a sweep racing a live publisher's final commit
could remove objects it is about to name. Sweeping an abandoned upload
**permanently retires its id** — the same tombstone a delete writes — so a
publisher resumed afterwards can neither claim the name nor make its swept
bytes visible; retry the publish under a fresh id. (The publisher also
re-checks the tombstone and its own upload marker around the commit, as
defense in depth.) Remember the multipart caveat above: cleanup removes
what listing can see; crashed multipart parts need the bucket lifecycle
rule.

`--store-id` stamps a store identity into every manifest, marker and
tombstone, and refuses reads, deletes and cleanups across identities, so
two stores sharing a prefix cannot touch each other's snapshots.

## Querying

```sh
traza-object query --snapshot nightly-2026-09-06 --service checkout \
    --since-ns 1757000000000000000 --limit 100 \
    --bucket my-archive --prefix traza --store-id prod-traza
traza-object trace --snapshot nightly-2026-09-06 --trace-id abc123 ...
traza-object payload --snapshot nightly-2026-09-06 --ref sha256/<hex> ...
```

Queries answer **exactly as the source store would have at pin time**:
last-write-wins across segments (a span overwritten in a later segment
answers its newest version), tenant scoping, sessions, content search,
sorting, cursors. `--deadline-ms` bounds a query's compute the way
`--query-deadline-ms` does locally — worth setting, since remote scans cost
money as well as time. A query never reports success after its budget
expired; precisely, the budget is checked at segment boundaries, every few
thousand records, and once before returning. An in-flight segment operation
can issue several requests before its next checkpoint, so this is not a
strict wall-clock cancellation bound. Remote requests have separate
`--op-timeout-secs` bounds; callers sharing a `Remote` also wait for its
serialized transport admission. The Rust API is
`traza::object_storage::Remote` / `RemoteSnapshot`.

Each query reports its transfer economics on stderr — bytes fetched versus
the archive's total — and `RemoteSnapshot::read_stats` exposes the same
numbers programmatically. Transfer counters accumulate across a `Remote`
and its clones, including publication read-back and administration. Use a
fresh `Remote` to measure a reader independently; residency figures belong
to the snapshot handle.

### Memory truth

Three residencies, none hidden:

- the **chunk cache** (`--cache-mb`, default 256) is a global byte budget
  across all of a snapshot's segments, LRU-evicted;
- each opened segment's **decoded metadata indexes** are eager and **not
  bounded by that cache** — exactly like a local open, they scale with
  index cardinality and live as long as the snapshot handle;
- each segment's small decoded-block cache is per-segment bounded, as
  locally.

Nothing hydrates whole objects to local disk.

## Verifying, restoring, deleting

```sh
traza-object verify  --snapshot nightly-2026-09-06 --deep ...
traza-object restore --snapshot nightly-2026-09-06 --into /restore/nightly ...
traza-server --data-dir /var/lib/traza-restored --restore /restore/nightly
traza-object delete  --snapshot nightly-2026-09-06 ...
```

- `verify` checks the manifest's binding to its archived generation and
  every object's existence and length; `--deep` re-reads and re-hashes
  everything.
- `restore` streams into an exclusively created staging directory
  (chunk-verified, then file-verified, then generation-verified with the
  engine's own manifest code), fsyncs the tree bottom-up, and commits by
  one rename. It refuses an existing target — checked up front and again
  immediately before the rename — and never follows symlinks. **The
  precondition is yours:** nothing else may create the target while the
  restore runs; `std` has no atomic no-replace directory rename, so the
  rechecks bound the race window rather than closing it. The result is a
  pin-equivalent directory carrying **every mutation domain** — spans,
  annotations, payload bytes, eval log, tombstone log.
- `delete` writes the permanent **tombstone first**, removes the manifest
  (new snapshot opens are refused), then every data object
  the snapshot owns. Idempotent and resumable — re-run an interrupted
  delete to finish it, from any client. It never touches keys outside
  `snapshots/<id>/` and never reports success while listed data objects remain;
  the tombstone is intentionally retained. Already-open handles can retain
  cached data and must be discarded when retiring the archive. In-flight
  reads may fail as objects disappear.
  A prefix with an intent marker and no manifest may be a publication in
  flight; `delete` always refuses it — that is `cleanup
  --publisher-quiescent`'s job.

## Remote layout

```text
<prefix>/snapshots/<id>/manifest.json      the snapshot, written LAST
<prefix>/snapshots/<id>/objects/NNNNNNNN.pack
<prefix>/snapshots/<id>/UPLOADING          upload intent; gone once published
<prefix>/snapshots/<id>/TOMBSTONE          deletion fence; written first, kept forever
```

## Limitations (preview)

- Manifests are limited to 64 MiB, with at most 200,000 files and 200,000
  pack objects per snapshot. Listing scans at most 200,016 objects across
  the archive prefix and fails explicitly above that limit. Partition large
  inventories into separate prefixes; known snapshot IDs remain addressable.
- No automatic scheduling, tiering, or retention of snapshots — publishing
  and deleting are explicit operator actions.
- Publication uploads sequentially (bounded, predictable, slower than a
  parallel uploader would be).
- The dashboard does not read archives; `traza-object query`/`trace` and
  the Rust API do.
- Analytics rollups are not archived; remote queries always resolve from
  segments (correct, but aggregate-shaped workloads cost more remotely).
- A pin taken while a client was mid-conversation archives exactly what a
  backup would: the pinned generation, nothing later.

## See also

- [Backup and restore](backup.md) — pins, generations, `--restore`
- [Durability](durability.md) — what a local acknowledgement means
- [Administration](administration.md) — erasures and their receipts
