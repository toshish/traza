# Storage cost: Traza next to OpenObserve and Elasticsearch

OpenObserve publishes a storage-cost table comparing itself with Elasticsearch
([`screenshots/zo_vs_es.png`](https://github.com/openobserve/openobserve/blob/main/screenshots/zo_vs_es.png)),
and it is the number their headline claim rests on: *140x lower storage cost*.
This document computes the same eight metrics for Traza.

The short version: **on ordinary span traffic Traza loses this comparison, and
loses it badly.** Per byte ingested it keeps about 7x more bytes than
Elasticsearch and about 98x more than OpenObserve — which becomes roughly
1,000x on the HA cost row, once the object-storage rate and the per-replica
multiplier are applied. On long-context agent traffic, the workload Traza is
actually built for, the same metrics come out **34x better than
Elasticsearch**. Both results are measured, and both are below.

## What OpenObserve published

| Metric | Elasticsearch | OpenObserve |
|---|---:|---:|
| Ingested data (MB) | 185,493 | 185,493 |
| Documents | 173,384,804 | 173,834,714 |
| Stored data (MB) | 52,152 | 3,891 |
| Compression ratio | 3.56 | 47.67 |
| Compression ratio to ES | 1 | 13.4 |
| Storage cost for 1 node ($) | 4 | 0.087 |
| Storage cost for 3-node HA cluster ($) | 12.2232 | 0.087 |
| Cost advantage to ES in HA mode | 1 | 140 |

Reading the arithmetic back out of the table: the cost rows price block storage
at $0.08/GiB-month and object storage at $0.023/GiB-month. Elasticsearch pays
three times in HA because each node keeps its own copy; OpenObserve pays once
because all three nodes read the same objects. That single architectural fact,
not compression, is what turns a 13.4x storage advantage into a 140x cost
advantage.

The corpus is Kubernetes-format log data — about 1,122 bytes per document.
No methodology is published beside the image itself; OpenObserve's later
[1.1 TB benchmark](https://openobserve.ai/blog/elasticsearch-openobserve-benchmarking/)
describes synthetic k8s logs fanned out to both systems through Fluent Bit.
The Elasticsearch and OpenObserve columns here are theirs, carried over
unverified. Only the Traza columns were measured for this document.

## What Traza measures

`cargo run --release --bin storage-bench` ingests each corpus through the real
HTTP path, counting the exact request-body bytes it sends, waits for the flush
and for compaction to settle, then walks the whole data directory. Full output,
including exact byte counts, is in [`storage.md`](benchmarks/storage.md).

| Corpus | Spans | Ingested | On disk | Ratio | Bytes/span stored |
|---|---:|---:|---:|---:|---:|
| `generic` — service traces | 1,000,000 | 311.9 MiB | 564.3 MiB | **0.55 : 1** | 592 |
| `llm` — LLM calls, ~2 KiB of text each | 200,000 | 438.7 MiB | 909.4 MiB | **0.48 : 1** | 4,768 |
| `pinned-context` — 320 KiB pinned context per call | 10,000 | 3,144.4 MiB | 26.3 MiB | **119.72 : 1** | 2,754 |

The first two ratios are below 1:1, which means they are not compression ratios
at all. Traza stores 1.8x to 2.1x *more* bytes than the client sent it.

## The same eight metrics, filled in

Traza's corpora are spans, not k8s logs, so the absolute megabyte counts are not
comparable across columns. What is comparable is each system's ratio of stored
bytes to ingested bytes. The table below therefore holds ingest volume fixed at
OpenObserve's 185,493 MB and applies each system's own measured ratio; the
document counts follow from each corpus's average document size.

| Metric | Elasticsearch | OpenObserve | Traza `generic` | Traza `llm` | Traza `pinned-context` |
|---|---:|---:|---:|---:|---:|
| Ingested data (MB) | 185,493 | 185,493 | 185,493 | 185,493 | 185,493 |
| Documents | 173,384,804 | 173,834,714 | 594,685,181 | 84,556,578 | 589,913 |
| Stored data (MB) | 52,152 | 3,891 | 331,081 | 382,646 | 1,533 |
| Compression ratio | 3.56 | 47.67 | 0.55 | 0.48 | 119.72 |
| Compression ratio to ES | 1 | 13.4 | 0.16 | 0.14 | 34.0 |
| Storage cost for 1 node ($) | 4.07 | 0.087 | 25.87 | 29.89 | 0.12 |
| Storage cost for 3-node HA cluster ($) | 12.22 | 0.087 | 77.60 | 89.68 | 0.36 |
| **Cost advantage to ES in HA mode** | **1** | **140** | **0.16** | **0.14** | **34.0** |

Traza pays the block-storage rate and pays it once per node, exactly as
Elasticsearch does: there is no shared-storage tier today, so an HA cluster
triples the bill. The `pinned-context` column beats Elasticsearch by 34x and
still loses to OpenObserve by 4x, because OpenObserve's HA row does not
multiply and Traza's does.

## Why the first two columns look like that

Nothing in Traza compresses. A segment is a header, then records, then four
indexes; each record carries its trace id, its attribute key/value pairs as
text, and the span's JSON payload verbatim. The attribute values are therefore
stored twice — once inside the payload, once in the record's key/value list —
and the record-offset, trace, attribute and content indexes are written on top
of that. Measured against the JSON a client sent, that is 1.8x on service
traces and 2.1x on LLM spans.

The amplification is not accidental; it is what the query numbers are bought
with. Trace lookup at p95 0.64 ms and attribute-filtered search at p95 3.3 ms
over a million spans come from those indexes, and content search returning in
1.5 ms against 1,258 ms of scanning comes from the per-block word filters that
cost the extra +0.1% on disk. OpenObserve's Parquet-plus-zstd blocks make the
opposite trade and then remove the HA multiplier entirely by putting the blocks
in S3. On the metric they chose to publish, that combination wins, and no
reading of these numbers changes that.

## Why the third column looks like that

Above `--payload-threshold-bytes` (256 KiB by default), a string attribute is
lifted out of the span into `payloads/<aa>/<sha256>.bin` and replaced by a
`{"$payload": …}` reference. The store is content-addressed, so a system prompt
or a pinned context repeated across ten thousand calls is written once. In the
`pinned-context` corpus, 3.1 GiB of ingested text becomes 328 KiB of payload
store plus 25.7 MiB of segments — the segments hold the references, the
per-call user text, and the indexes.

This is a real property of long-context agent traffic, not a benchmark trick:
pinned system prompts and cached context blocks are byte-identical by
construction, which is why providers bill them as cache reads. But the win is
exactly as narrow as that: **it requires byte-identical values above the
threshold.** A context that varies per call, or one that sits below 256 KiB —
the `llm` corpus, at ~2 KiB per span — deduplicates nothing and lands back in
the first two columns.

## What compression would buy

**Traza had no compressor when these numbers were taken.** Three dependencies
(`lz4_flex` is the one [format v7](segment-format.md) added), no `zstd`, no
`--compress` flag, and the [ingest guide](guide/ingest.md) asks OTel exporters
for `OTEL_EXPORTER_OTLP_COMPRESSION=none` because the server cannot decode a
compressed body either. Everything in this section is therefore a **projection
measured before v7 existed** — computed with `zstd -3` at 64 KiB blocks where
v7 ships LZ4 at 128 KiB — and belongs in a different mental column from every
other number in this document; v7's own acceptance gates, not these tables,
are what its implementation is held to.

It is projected from the segment files `storage-bench` actually produced, not
from a re-creation of them. Only the **records region** is compressed: the
104-byte header and the four index sections stay raw, because the reader parses
them at open and probes them per query, so compressing them would flatter the
number and break the design. Two variants — the whole region as one stream (an
unreachable bound: reading one record would decompress everything before it),
and 64 KiB blocks, each independently decompressible, which is what an
implementation would actually do.

| Corpus | Records | zstd -3, whole region | zstd -3, 64 KiB blocks | Segment total | Shrink |
|---|---:|---:|---:|---:|---:|
| `generic` | 498.5 MiB | 18.2 MiB | 30.2 MiB | 556.7 → 88.4 MiB | **6.30x** |
| `llm` | 862.3 MiB | 12.8 MiB | 35.2 MiB | 905.1 → 78.1 MiB | **11.59x** |
| `pinned-context` | 23.6 MiB | 0.4 MiB | 0.7 MiB | 25.7 → 3.1 MiB | **9.27x** |

`zstd -12` buys a further 1–8%; `gzip -6` is 7–10% worse. Block-wise costs 66%
more than whole-region on `generic` — that is the price of random access, and it
is the column to read.

Folded back into the comparison, block-wise `zstd -3` moves the headline row:

| Cost advantage to ES in HA mode | `generic` | `llm` | `pinned-context` |
|---|---:|---:|---:|
| As measured today | 0.16 | 0.14 | 34.0 |
| Projected with compression | **0.99** | **1.58** | **287** |

Compression takes Traza from six times worse than Elasticsearch to **level with
it**, and stops there; OpenObserve stays 13.5x ahead. The reason is visible in
the first table: once the records compress, **the indexes are the bulk** — 66%
of the compressed `generic` segment, 55% for `llm`, 75% for `pinned-context`.
Past Elasticsearch parity the work is in the indexes, not the payload.

Three costs this projection excludes, all of them real:

- **The corpora are synthetic and templated.** 6.30x on `generic` sits inside
  the 4–8x band row stores reach on real logs, so that one is plausible. 11.59x
  on `llm` leans on a byte-identical system prompt in every span; real agent
  traffic does repeat system prompts, but varies far more in the user turn.
- **Decompression on the read path is not measured.** A 64 KiB block costs tens
  of microseconds at `zstd -3` speeds, which is not a rounding error against a
  0.37 ms trace lookup or a 2.1 ms filtered search.
- **`zstd` would be a fourth dependency**, in a project whose policy is that
  new dependencies need a reason. v7 spent its dependency on `lz4_flex`
  instead — the argument is in
  [internals/dependencies.md](internals/dependencies.md) — so these `zstd`
  ratios remain an upper reference, not what shipped. Hand-rolling a
  compressor is a serious lift.

Reproduce the inputs with `TRAZA_STORAGE_BENCH_KEEP=1 cargo run --release --bin
storage-bench`, which leaves each corpus's data directory on disk; the header
gives the records region at bytes 24 and 32 (see
[segment format](segment-format.md)).

Columnar segment projections and an object-storage tier attack what
compression cannot.
The object tier is the one that matters most for the cost row, because it is the
only thing that removes the per-replica multiplier. Neither has shipped.
Compression is now specified — the
[v7 format](segment-format.md#format-v7),
targeted at v0.24.0 — but remains unimplemented, and every number in this
section stays a projection until it ships and is measured.

## What these eight metrics do not measure

Storage cost per ingested byte is one axis, and every number in Traza's favour
sits on a different one. From the bundled benchmarks on macOS/aarch64:

- **Ingest:** 116,618 spans/s from one client, 208,973 spans/s at 16 concurrent
  clients, at the default `wal` durability ([`ingest.md`](benchmarks/ingest.md))
- **Query:** trace lookup p95 0.64 ms, attribute-filtered search p95 3.3 ms over
  1M spans ([`canonical-corpus.md`](benchmarks/canonical-corpus.md))
- **Content search:** 1.5 ms versus 1,258 ms scanning, for +0.1% on disk
- **Footprint:** a 3.3 MiB binary, three direct dependencies, no external
  database, queue or coordinator; a store larger than RAM serves correctly
- **LLM/agent semantics:** sessions, token and cost analytics, annotations and
  MCP are first-class rather than dashboards built over generic logs

None of that offsets a three-order-of-magnitude HA storage bill for an operator
whose constraint is storage cost. It is a different constraint, and the honest
framing is that Traza and OpenObserve are optimised for different ones.

## Reproducing this

```bash
cargo run --release --bin storage-bench
```

Corpus sizes are overridable — `TRAZA_STORAGE_BENCH_GENERIC_SPANS`,
`TRAZA_STORAGE_BENCH_LLM_SPANS`, `TRAZA_STORAGE_BENCH_PINNED_CONTEXT_SPANS` —
and the run rewrites [`storage.md`](benchmarks/storage.md) with
exact byte counts, so every derived figure in this document can be recomputed
rather than trusted.
