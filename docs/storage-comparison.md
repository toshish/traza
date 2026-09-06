# Storage efficiency and deployment cost

Format v8 reduces persisted index bytes while preserving span contents and
query semantics. The defensible comparison is Traza against Traza on the
same corpus. Earlier versions of this page compared compression ratios from
different workloads across databases; those ratios cannot establish a
competitive advantage and have been removed.

## Measured v7 → v8 storage

`storage-bench` sends deterministic request bodies through the HTTP API and
counts the whole settled data directory, including segments, WAL, payloads,
manifests, and sidecars.

| Corpus | Spans | v7 bytes | v8 bytes | Reduction | v8 ingested:stored |
|---|---:|---:|---:|---:|---:|
| Service traces (`generic`) | 1,000,000 | 132,663,519 | 73,968,447 | 44.2% | 4.42:1 |
| LLM calls (`llm`) | 200,000 | 107,692,288 | 73,909,976 | 31.4% | 6.22:1 |
| Repeated large context (`pinned-context`) | 10,000 | 4,283,487 | 2,650,416 | 38.1% | 1244.02:1 |

The [generated record](benchmarks/storage.md) contains exact ingested bytes
and section measurements. The [paired comparison](benchmarks/storage-v8-comparison.md)
records latency, throughput, and full-export migration verification of all
1.21 million spans.

All three corpora contain templated synthetic text. The last ratio requires
byte-identical strings above the payload extraction threshold (256 KiB by
default); its single 320 KiB context also compresses unusually well. It does
not predict compression for arbitrary agent traffic. Measure representative
production samples before sizing storage.

## Where the savings come from

Format v7 compressed record blocks and payload blobs. Format v8 compacts the
record-offset, trace, and attribute indexes with delta varints and
LZ4-when-smaller sections. Checksums cover headers and pruning metadata;
content-filter matrix pages are checked again when read. Record and payload
encodings remain unchanged. The [format specification](segment-format.md)
describes the exact layout and migration contract.

The decoded in-memory indexes are unchanged: this release makes no RAM
reduction claim. Sidecars also remain unchanged, so they account for a larger
fraction of a smaller store. Migration and compaction temporarily need old
and new files; provision additional disk and take a full pre-upgrade backup.

## Storage placement is a separate cost decision

Traza currently has one writer and local storage. It does not provide native
replication, automatic failover, shared object storage, or a supported
three-node HA cluster. Three independent copies are not an HA implementation.

For planning, a simplified storage-only model is:

```
monthly storage cost = retained bytes / 2^30 × copies × dollars per GiB-month
```

For example, **assuming**, rather than quoting current prices, $0.08 per
GiB-month for block storage and $0.023 for object storage, three block copies
of the *same stored volume* cost about 10.4 times one object copy. This is
arithmetic under explicit assumptions, not a Traza deployment benchmark or a
comparison with another database. Provider durability is already part of a
storage product's price; application replication requirements must be modeled
separately.

Actual cost also includes provisioned capacity, temporary rewrite space,
backups, caches, compute, object requests, networking, retention, and the
consistency and recovery guarantees required by the application. A storage
format improvement addresses only part of that cost.

No same-corpus Elasticsearch or OpenObserve benchmark was performed for this
release. We therefore make no comparative compression, performance, or HA
cost claim. Shared object storage is a future architecture decision, with
its own coordination, caching, durability, and portability tradeoffs.

## Reproducing this

```bash
cargo run --release --bin storage-bench
```

A default-size run checks the storage acceptance gate and rewrites
[`storage.md`](benchmarks/storage.md). Run it on both releases for a paired
comparison. Size overrides (`TRAZA_STORAGE_BENCH_GENERIC_SPANS`,
`TRAZA_STORAGE_BENCH_LLM_SPANS`, `TRAZA_STORAGE_BENCH_PINNED_CONTEXT_SPANS`)
print exploratory results without replacing the default published record.
