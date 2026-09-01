# Storage cost: Traza next to OpenObserve and Elasticsearch

OpenObserve publishes a storage-cost table comparing itself with Elasticsearch
([`screenshots/zo_vs_es.png`](https://github.com/openobserve/openobserve/blob/main/screenshots/zo_vs_es.png)),
and it is the number their headline claim rests on: *140x lower storage cost*.
This document computes the same eight metrics for Traza.

The short version: **Traza stores fewer bytes than the client sends it** —
measured at 2.47:1 on service traces and 4.27:1 on LLM calls under
[segment format v7](segment-format.md), which compresses the records region
and stopped storing attribute value text twice. (Both corpora are templated
synthetic text; real traffic, which varies far more in the user turn, will
compress less — 4.27:1 is the favorable end.) That is 0.69x of
Elasticsearch's published ratio on service traces and 1.20x on LLM calls; on
long-context agent traffic, the workload Traza is built for, the same metrics
come out **216x better than Elasticsearch**. Every cross-system ratio in this
document carries one caveat: the Elasticsearch and OpenObserve ratios were
measured on k8s logs, Traza's on its span corpora, and how Elasticsearch
would compress LLM-call JSON is measured by neither document.
What has not moved is the architecture of the cost row: Traza pays the
block-storage rate once per node, OpenObserve reads shared objects, and on
the HA row that fact — not compression — keeps OpenObserve far ahead on
ordinary traffic. The Traza columns are measured, and everything is below;
the Elasticsearch and OpenObserve halves of every comparison are their
published figures, carried over unverified. (An earlier edition of this
document was measured on the uncompressed v6 format, which stored 1.8–2.1x
the bytes ingested; those numbers survive in this file's git history and in
the [format document's motivation](segment-format.md#format-v7).)

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
and for compaction to settle, then walks the whole data directory. The run
asserts [acceptance gate 5](segment-format.md#acceptance-gates) before it will
write its record. Full output, including exact byte counts, is in
[`storage.md`](benchmarks/storage.md).

| Corpus | Spans | Ingested | On disk | Ratio | Bytes/span stored |
|---|---:|---:|---:|---:|---:|
| `generic` — service traces | 1,000,000 | 311.9 MiB | 126.5 MiB | **2.47 : 1** | 133 |
| `llm` — LLM calls, ~2 KiB of text each | 200,000 | 438.7 MiB | 102.8 MiB | **4.27 : 1** | 539 |
| `pinned-context` — 320 KiB pinned context per call | 10,000 | 3,144.4 MiB | 4.1 MiB | **769.74 : 1** | 428 |

All three ratios are above 1:1: the store keeps 0.41x, 0.23x, and 0.0013x of
the bytes it was sent. The v6 format measured 1.81x and 2.07x on the first two
corpora — amplification, not compression — and the difference is what
[format v7](segment-format.md#format-v7) shipped for.

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
| Stored data (MB) | 52,152 | 3,891 | 75,242 | 43,470 | 241 |
| Compression ratio | 3.56 | 47.67 | 2.47 | 4.27 | 769.74 |
| Compression ratio to ES | 1 | 13.4 | 0.69 | 1.20 | 216 |
| Storage cost for 1 node ($) | 4.07 | 0.087 | 5.88 | 3.40 | 0.02 |
| Storage cost for 3-node HA cluster ($) | 12.22 | 0.087 | 17.63 | 10.19 | 0.06 |
| **Cost advantage to ES in HA mode** | **1** | **140** | **0.69** | **1.20** | **216** |

Traza pays the block-storage rate and pays it once per node, exactly as
Elasticsearch does: there is no shared-storage tier today, so an HA cluster
triples the bill. On service traces Traza now sits at 0.69x of Elasticsearch —
within reach, no longer 6x behind — and on LLM calls it is 1.20x ahead. The
`pinned-context` column's 216 exceeds even OpenObserve's headline 140. All of
these cross-system rows carry the different-corpora caveat from the top of
this document — their ratios measured on k8s logs, Traza's on its own span
corpora, and the `pinned-context` column on the repeated-context workload
content addressing exists for. On ordinary traffic OpenObserve's HA row stays
117–203x ahead of Traza's two columns, and the reason is the row above it:
their three nodes share one copy of the objects and pay the object rate for
it.

## Why the first two columns look like that

A v7 segment compresses its records region — 128 KiB record-aligned LZ4
blocks, per-block CRC, a resident block directory — and a record carries a
fixed 20-byte `(key id, value digest)` pair per attribute instead of the key
and value text, so the value text is stored once, in the payload, rather than
twice. Payload blobs compress under the same codec-or-raw rule. Measured
against the JSON a client sent, the store keeps 0.41x on service traces and
0.23x on LLM spans, where v6 kept 1.81x and 2.07x.

One honesty note survives the projection's retirement on purpose: **the
corpora are templated synthetic text**. The `llm` corpus repeats a
byte-identical system prompt in every span and templates its user and
completion turns — its records compress to 0.11 of their logical bytes in
[the records-region table](benchmarks/storage.md#records-region-measurements)
— and real agent traffic does repeat system prompts, but varies far more in
the user turn, so it will compress less than this corpus does. 0.23x is the
favorable end, not a number to expect from arbitrary traffic; measure your
own (the [reproduction recipe](#reproducing-this) is below).

What remains on disk is roughly half indexes, by design. The record-offset,
trace, attribute and content indexes are stored raw: the reader parses them at
open and probes them per query, and compressing them would put a decode in
front of every probe. The [records-region table in
storage.md](benchmarks/storage.md#records-region-measurements) puts the
compressed records at 51.0% of segment bytes on `generic`, 56.6% on `llm` and
45.4% on `pinned-context` — the rest is header and index sections. Those
indexes are what the query numbers are bought with: trace lookup at p95
0.86 ms and attribute-filtered search at p95 4.4 ms over a million spans
([canonical-corpus.md](benchmarks/canonical-corpus.md)), and content search
returning a selective term in 1.5 ms against 1,258 ms of scanning
([capacity.md](operations/capacity.md#content-search)). Shrinking the indexes
is future index-format work, not a codec knob.

OpenObserve's Parquet-plus-zstd blocks in S3 make a different trade and then
remove the HA multiplier entirely by putting the blocks in object storage.
Where a columnar engine writing to object storage still wins, it wins on
architecture Traza does not have: the object-storage rate paid once for the
whole cluster, and scan-heavy analytics over columns, which a row-shaped
records region plus point indexes is not built for. On the metric they chose
to publish, that combination keeps the ordinary-traffic row theirs.

## Why the third column looks like that

Above `--payload-threshold-bytes` (256 KiB by default), a string attribute is
lifted out of the span into `payloads/<aa>/<sha256>.bin` and replaced by a
`{"$payload": …}` reference. The store is content-addressed, so a system prompt
or a pinned context repeated across ten thousand calls is written once — and
under v7 the blob is itself LZ4-compressed, with its content address unchanged
(the SHA-256 of the uncompressed bytes). In the `pinned-context` corpus,
3.1 GiB of ingested text becomes 5,961 bytes of payload store plus 3.8 MiB of
segments — the segments hold the references, the per-call user text, and the
indexes. The 5,961 bytes are one 320 KiB context stored once and then
compressed ~55:1; that last ratio is a property of this corpus's
paragraph-templated synthetic text, not a number to expect from a real
context. Storing it once is the property that generalizes.

This is a real property of long-context agent traffic, not a benchmark trick:
pinned system prompts and cached context blocks are byte-identical by
construction, which is why providers bill them as cache reads. But the win is
exactly as narrow as that: **it requires byte-identical values above the
threshold.** A context that varies per call, or one that sits below 256 KiB —
the `llm` corpus, at ~2 KiB per span — deduplicates nothing and lands back in
the first two columns.

## What compression bought, next to the projection that motivated it

Before v7 shipped, this section carried a projection: `zstd -3` over the v6
records region at 64 KiB blocks, computed from the segment files
`storage-bench` had actually produced. v7 shipped LZ4 at 128 KiB blocks
instead — ratio traded for decode speed and safety of implementation, argued
in [internals/dependencies.md](internals/dependencies.md) — and also removed
the attribute-value double-store, which the projection could not model. Both
are now on the record:

| Corpus | v6 segments | Projected (zstd -3, 64 KiB blocks) | v7 measured (LZ4, 128 KiB blocks) | v7 shrink |
|---|---:|---:|---:|---:|
| `generic` | 556.7 MiB | 88.4 MiB (6.30x) | 118.9 MiB | **4.68x** |
| `llm` | 905.0 MiB | 78.1 MiB (11.59x) | 98.4 MiB | **9.20x** |
| `pinned-context` | 25.7 MiB | 3.1 MiB (9.27x) | 3.8 MiB | **6.76x** |

The projection was labeled an optimistic reference measured under different
knobs, and it was: `zstd -3` at finer blocks projects 18–26% smaller segments
than LZ4 delivered, even with the double-store removal on LZ4's side. The
shrink column compares format to format, not codec to codec — v7 changed the
records and then compressed them. The projection's other conclusion
held in direction and overshot in degree: once the records compress, the
indexes are roughly half of segment bytes — measured at 43–55% by corpus (the
complement of the records shares above), where the projection's tables put
them at 55–75% — and past this point the work is in the indexes, not the
payload.

Two things this outcome leaves unimplemented, both deliberate:

- **A zstd cold tier remains evidence-gated.** The dependency ledger
  ([internals/dependencies.md](internals/dependencies.md)) states the gate; the
  ~20% ratio gap in the table above is the evidence a proposal would weigh
  against zstd's cost. If it ever clears, it is a new codec id under the v7
  header, not a format bump.
- **An object-storage tier has not shipped**, and it is the only thing that
  removes the per-replica multiplier from the cost row. Compression moved the
  stored-bytes rows; the HA row's remaining gap belongs to architecture.

One piece of guidance survives from the no-compressor era unchanged: the
[ingest guide](guide/ingest.md) still asks OTel exporters for
`OTEL_EXPORTER_OTLP_COMPRESSION=none`. That is not about storage — the server
does not decode `Content-Encoding` on request bodies, so a gzip-compressed
OTLP body fails parsing and is refused. v7's LZ4 compresses what the store
keeps, not what the exporter sends; wire compression remains unimplemented,
and the guidance stands.

## What these eight metrics do not measure

Storage cost per ingested byte is one axis, and every number in Traza's favour
used to sit on a different one; after v7, storage itself is contested on two of
the three corpora, and the others still stand. From the bundled benchmarks on
macOS/aarch64:

- **Ingest:** 82,805 spans/s from one client, 208,973 spans/s at 16 concurrent
  clients, at the default `wal` durability —
  [`ingest.md`](benchmarks/ingest.md)'s engine-limit rows (quiet machine,
  compaction off), measured 2026-07 on the v6-format build; gate 6's
  interleaved A/B measured v7 ingest no slower
  ([segment-format.md](segment-format.md#acceptance-gates))
- **Query:** trace lookup p95 0.86 ms, attribute-filtered search p95 4.4 ms over
  1M spans ([`canonical-corpus.md`](benchmarks/canonical-corpus.md))
- **Content search:** 1.5 ms versus 1,258 ms scanning, for +0.1% on disk — a
  [format-v5-era record](operations/capacity.md#content-search); the v7 scan
  side pays block decode and has not been re-measured
- **Footprint:** a 3.4 MiB binary, three direct dependencies, no external
  database, queue or coordinator; a store larger than RAM serves correctly
- **LLM/agent semantics:** sessions, token and cost analytics, annotations and
  MCP are first-class rather than dashboards built over generic logs

For an operator whose binding constraint is HA storage cost on ordinary span
traffic, OpenObserve's object-storage economics still win by two orders of
magnitude, and nothing above offsets that. It is a different constraint, and
the honest framing is unchanged: Traza and OpenObserve are optimised for
different ones.

## Reproducing this

```bash
cargo run --release --bin storage-bench
```

A default-size run rewrites [`storage.md`](benchmarks/storage.md) with exact
byte counts, so every derived figure in this document can be recomputed
rather than trusted, and it gates itself: it asserts settled amplification at
or below 1.0x on `generic` and `llm` (acceptance gate 5 of
[the segment format](segment-format.md#acceptance-gates)) after measuring and
before writing, and a run that misses exits non-zero with nothing written.
Corpus sizes are overridable — `TRAZA_STORAGE_BENCH_GENERIC_SPANS`,
`TRAZA_STORAGE_BENCH_LLM_SPANS`, `TRAZA_STORAGE_BENCH_PINNED_CONTEXT_SPANS` —
but an overridden run is an experiment, the same rule the other harnesses
hold: it prints its table, does not assert the gate (which is defined at the
default sizes only), and leaves the published record alone.
