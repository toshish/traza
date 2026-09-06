# Format v8 against v7: the measured before/after

This page is a hand-written record, unlike its generated neighbours
([`storage.md`](storage.md), [`canonical-corpus.md`](canonical-corpus.md),
[`query.md`](query.md)), which are rewritten by their harnesses and only ever
hold the latest run. Its job is the one thing those files cannot show: the
same harnesses run **twice in one session on one machine** — once on the last
v7 build, once on the v8 build — with the numbers from both sides transcribed
here unedited. Every figure below was produced by a bundled benchmark or by
the export oracle described in its section; nothing is estimated.

**Sources.** Baseline: v0.24.2 at `016eab8` (segment format v7). Candidate:
v0.25.0 at `7ec2183` (segment format v8). Machine: macOS/aarch64 laptop, 10
hardware threads, local SSD, not otherwise idle — the honesty notes below say
where that shows. Shipped defaults throughout; corpus generation is
deterministic, so both sides ingested **byte-identical request bodies**.

## Storage: whole data directory, fresh ingest

`storage-bench`, default corpora (1,000,000 / 200,000 / 10,000 spans). "On
disk" is a recursive walk of the whole settled data directory — segments,
write-ahead log, payload store, generation manifests, sidecars — not segments
alone. Exact bytes, identical ingested bytes on both sides:

| Corpus | Ingested (bytes) | v7 on disk | v8 on disk | Reduction | v8 ratio (in:stored) |
|---|---:|---:|---:|---:|---:|
| `generic` | 327,069,707 | 132,663,519 | 73,968,447 | **−44.2%** | 4.42 : 1 |
| `llm` | 460,055,297 | 107,692,288 | 73,909,976 | **−31.4%** | 6.22 : 1 |
| `pinned-context` | 3,297,157,900 | 4,283,487 | 2,650,416 | **−38.1%** | 1244.02 : 1 |

Segment files alone: 124,656,069 → 65,960,997 (`generic`), 103,011,908 →
69,229,596 (`llm`), 3,979,924 → 2,346,853 (`pinned-context`); the WAL,
payload store, and "other" bytes are identical on both sides. The v8 run is
the one published in [`storage.md`](storage.md), where gate 5 (settled
amplification ≤ 1.0x) was asserted before writing.

The reduction comes from the index sections — v8 stores record offsets and
trace/attribute postings as delta varints inside checksummed,
LZ4-when-smaller section wrappers — while the records region and payload
blobs keep v7's encoding byte-for-byte. All three corpora are **templated
synthetic text**; the reductions are measured facts about these workload
shapes, not a forecast for arbitrary traffic. Measure your own corpus with
the reproduction commands below.

## Migration: nothing lost, byte-verified

An export oracle, not a row count: each of the three v7 baseline stores was
opened once by the v8 build (triggering the automatic v6/v7 → v8 migration),
and the **full normalized JSON export of every span** — 1,210,000 across the
three stores — was compared against the same export taken from the v7 store
before migration.

| Store | Spans | Export SHA-256 unchanged | `/v1/verify` after migration |
|---|---:|---|---|
| `generic` | 1,000,000 | yes (`154ba488…`) | intact |
| `llm` | 200,000 | yes (`50bae8bc…`) | intact |
| `pinned-context` | 10,000 | yes (`8c83f010…`) | intact |

Migrated on-disk totals (73,989,866 / 73,772,340 / 2,650,891 bytes) sit
within a fraction of a percent of the fresh-ingest v8 totals above but are
not identical, because a migrated store carries its pre-migration
segment/manifest generation history where a fresh store does not. **Do not
conflate the two measurements**: the table above is fresh ingest on both
sides; this section is conversion of the v7 bytes.

Migration is automatic at first open and **one-way from the first converted
file** — take a backup first. The authoritative contract, including the
record-stream completeness rule and the rollback boundary, is
[the format document's migration section](../segment-format.md#migration-v6-and-v7--v8).

## Latency and throughput: paired runs, honestly

`bench` (the canonical 1M-span corpus) was run twice per side, interleaved in
one session. All four runs passed every gate — ingest ≥ 50,000 spans/s,
trace p95 < 50 ms, filter p95 < 300 ms, and gate 6's p50 tripwires. The
committed [`canonical-corpus.md`](canonical-corpus.md) is the latest (repeat)
candidate run, not a picked one.

| Run | Ingest | Trace p50 / p95 / p99 | Filter p50 / p95 / p99 |
|---|---:|---|---|
| v7, first | 66,568/s | 0.388 / 0.802 / 1.522 ms | 3.361 / 5.544 / 6.413 ms |
| v8, first | 69,615/s | 0.346 / 1.149 / 2.322 ms | 3.160 / 5.498 / 7.600 ms |
| v7, repeat | 68,120/s | 0.380 / 1.343 / 1.548 ms | 3.200 / 6.594 / 10.512 ms |
| v8, repeat | 65,749/s | 0.577 / 1.280 / 2.094 ms | 3.657 / 5.443 / 6.468 ms |

All gates passed, but two runs per build on a shared laptop are insufficient
to establish performance equivalence or a speedup. Trace-lookup **p99** was
higher in both v8 runs: 2.322/2.094 ms versus 1.522/1.548 ms on v7. Its cause
was not isolated by this experiment. The 50 ms trace gate applies to **p95**,
which passed in every run; it is not a p99 acceptance threshold.

**Aggregation** (`query-bench`, 1,000,000 spans, 8 concurrent ingest clients):
both sides completed cold-after-restart, warm, and windowed queries, with
concurrent ingest and a dashboard probe during ingest/flush/settle. Every
answer was checked for correctness. The v8 run observed zero merge events,
so its probe does not establish latency during actual compaction. The
committed [`query.md`](query.md) is the v8 run.

**Content search** (`content-bench`, 100,000 spans, 60-word prompt and
completion each, 50 segments, both sides on the same loaded machine):

| | v7 | v8 |
|---|---:|---:|
| Rare term (1 match) | 0.555 ms | 0.570 ms |
| Selective pair (1 match) | 0.556 ms | 0.613 ms |
| Common term (97,777 matches) | 412.495 ms | 394.502 ms |
| Absent term | 0.005 ms | 0.011 ms |
| Indexed store, segment bytes | 46.9 MiB | 41.5 MiB |
| Resident index | 27.08 MiB | 27.08 MiB |

Disk shrinks ~11% on this corpus; selective lookups stay sub-millisecond
with the v8 content matrix's per-page checksum verification on the read
path. Resident memory is unchanged — **v8 is a disk-efficiency release and
makes no RAM-reduction claim**: the decoded in-memory index structures are
the same as v7's.

## Reproducing this

Each command rewrites (or, for `content-bench`, prints) its own record and
self-asserts its gates; run them on both a v0.24.2 and a v0.25.0 checkout to
recreate the pairing:

```bash
cargo run --release --bin storage-bench   # rewrites storage.md, asserts gate 5
cargo run --release --bin bench           # rewrites canonical-corpus.md, asserts gate 6 tripwires
cargo run --release --bin query-bench     # rewrites query.md
cargo run --release --bin content-bench   # prints its table; no committed record
```

For the migration oracle: `GET /v1/export` from a v0.24.2 server over a v7
store, open the same directory once with v0.25.0 (back it up first —
migration is one-way), export again, normalize each JSON object with sorted keys and compact UTF-8 encoding,
preserve array and export-record order, append a newline per record, and compare
digests; then `GET /v1/verify` on the migrated store. The repository also
carries a frozen v7 fixture store with pins, blobs, and a WAL tail
([`tests/fixtures/v8-migration`](../../tests/fixtures/v8-migration)) that the
migration test suite opens on every CI run.

## What this page does not claim

- The corpora are templated synthetic shapes; the 31–44% reductions are
  measured on them, not a per-customer forecast.
- One machine, one session, one or two runs per cell; no cross-machine or
  long-horizon variance is measured.
- The high-volume tables elsewhere (10M/100M-span compaction and memory
  matrices in [capacity.md](../operations/capacity.md)) predate v8 and were
  not re-run for this comparison.
- No same-corpus competitive benchmark was performed.
  [Storage economics](../storage-comparison.md) separates measured bytes from
  hypothetical deployment cost.
