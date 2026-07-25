# Capacity and performance

Every number on this page comes from a committed measurement record. None is
estimated, and none is carried over from a version that no longer exists.
Where a figure's configuration is not recorded, this page says so rather than
guessing.

**Run the benchmarks on your own hardware.** These were measured on one
machine; the shapes generalize, the absolute numbers do not. See
[benchmarking](../internals/benchmarking.md).

Sources:

- [`BENCHMARKS.md`](../../BENCHMARKS.md) — rewritten by
  `cargo run --release --bin bench`, the 1,000,000-span canonical corpus
- [`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md) — rewritten by
  `cargo run --release --bin ingest-bench`, the ingest matrix
- [`CHANGELOG.md`](../../CHANGELOG.md) — the 10M and 100M scaling runs, recorded
  with the release that measured them

## The canonical corpus

From [`BENCHMARKS.md`](../../BENCHMARKS.md): 1,000,000 spans over the real HTTP
path, on macOS/aarch64 with 10 hardware threads.

| Metric | Measured | Project target | Result |
|---|---:|---:|---|
| Sustained batched HTTP ingest | 116,618 spans/s | ≥ 50,000 spans/s | PASS |
| Trace-by-id p95 | 0.642 ms | < 50 ms | PASS |
| Attribute-filtered query p95 | 3.344 ms | < 300 ms | PASS |

| Query | p50 | p95 | p99 | samples |
|---|---:|---:|---:|---:|
| Trace by ID | 0.314 ms | 0.642 ms | 1.270 ms | 200 |
| Attribute filter | 1.762 ms | 3.344 ms | 7.242 ms | 100 |

That corpus occupied 583,182,107 bytes on disk across 100 segments — roughly
583 bytes per span *for this benchmark's span shape*. Your bytes-per-span
depends entirely on your attribute volume, so treat it as a method rather than
a constant: ingest a representative sample and read `bytes_on_disk` from
[`GET /v1/stats`](../guide/http-api.md#get-v1stats).

Note that JSON generation sits **inside** the timed ingest loop, so the ingest
rate includes client serialization and loopback HTTP overhead. It is a floor on
what the server can do, not a ceiling.

## Ingest throughput and concurrency

From [`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md): 1,000,000 spans per
run, 1,000 spans per batch, `wal` durability, median of 5 runs, on macOS/aarch64
(Apple M1 Max, 10 hardware threads).

| Scenario | Concurrency | Median spans/s |
|---|---:|---:|
| HTTP JSON, keep-alive, `wal` | 1 | 82,805 |
| HTTP JSON, keep-alive, `wal` | 4 | 155,685 |
| HTTP JSON, keep-alive, `wal` | 8 | 193,188 |
| HTTP JSON, keep-alive, `wal` | 16 | **208,973** |
| Direct engine (no HTTP), `wal` | 8 | 193,637 |
| Direct engine (no HTTP), `buffered` | 8 | 313,201 |

Two readings matter more than the peak:

**HTTP is not the constraint.** The direct-engine number at concurrency 8
(193,637) and the HTTP JSON number at the same concurrency (193,188) are the
same figure. Removing HTTP entirely — no socket, no parsing, no protocol —
changes nothing. Whatever limits ingest is inside the engine.

**The writer lock is the ceiling, and where it sits depends on
`--flush-spans`.** The stage breakdown in
[`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md#the-limiting-stage) shows the
work performed while holding the writer lock at ~88% of the run's wall clock at
the default `--flush-spans 10,000`, which puts the ceiling near 218,000
spans/s **at that setting**. An earlier version of this page called that a hard
ceiling for the design; it is not. A seal carries a fixed cost — two fsyncs, a
create and rename, a reopen-and-parse — on top of its per-span cost, so sealing
less often amortizes the fixed part:

| | `--flush-spans` 10,000 | `--flush-spans` 30,000 |
|---|---:|---:|
| Seals over 1M spans / mean seal | 100 / 34.5 ms | 33 / 74.6 ms |
| Work done holding the writer lock | 4,581 ms | 3,336 ms |
| Share of wall clock | 88% | 80% |
| Implied ceiling | ~218,300/s | ~299,800/s |

Tripling the spans per seal raised mean seal time only 2.17x, not 3x. Solving
those two measured points gives roughly 14.4 ms fixed per seal plus 2.0 µs per
span — *derived from the two measurements above, not independently measured.*

**Against the 250,000 spans/s target:** `--profile throughput` at concurrency
16 measured a median of **250,453 spans/s** (min 122,768, max 261,215), and a
second run 261,782. The medians clear the target; the minima do not, and the
spread is contention on a machine that never went idle. The target is therefore
recorded as **not yet confirmed — re-measure on an idle machine**, not as met.
What is established is that the shortfall was never a property of the design;
most of it was one default.

**Durability is the biggest single lever**, which is exactly what the mode names
promise: `buffered` at 313,201 against `wal` at 193,637 in the same
direct-engine scenario.

**Protobuf versus JSON.** The controlled comparison now exists: holding the
route fixed at `/v1/traces`, optimized OTLP protobuf decodes at 479 ns/span
against optimized OTLP JSON at 1,275 ns/span — **2.7x faster** — on payloads
2.9x smaller (117 vs 342 bytes/span). Protobuf was never slower as a wire
format; it was slower as one implementation of one, and that implementation is
fixed. This barely moves end-to-end throughput, because decode is ~1.9% of
ingest cost and the writer lock reabsorbs the difference above concurrency 1.
Prefer protobuf for wire size and CPU; use whichever your exporter speaks.

## Memory

**The rule is structural, and it is the reason large stores work at all:
memory is O(indexes), not O(data).** Segments are file-backed. An open segment
holds a file handle plus its parsed indexes; record payloads are read from disk
by exact byte range on demand and are never retained — not even by the flush
that just wrote them. Two engine hooks pin this and the test suite asserts
them: resident persisted span structs and resident payload bytes are both zero
after open and after flush.

The practical consequence: **a store larger than RAM serves correctly.** Disk
latency applies to cold reads.

Recorded figures:

| Corpus | Configuration | Peak RSS |
|---|---|---:|
| 10M spans (~6 GB on disk) | Not recorded alongside the figure | 0.25 GB |
| 100M spans (~55 GB on disk) | Compaction disabled | 0.43 GB |
| 100M spans | Default compaction, 256 MiB cap | 2.0 GB |
| 100M spans | 1 GiB segment cap | 6.7 GB |

The 100M rows come from the 0.16.0 [changelog](../../CHANGELOG.md) entry and
were sampled directly during the run at 20-second intervals. The 10M row was
recorded without its compaction setting, so it is reported here without one.

Note the direction: **RSS rises with compaction, not with corpus size.** A merge
materializes its inputs, so the working set tracks the segment-size cap. The
baseline for serving is small; the peak is a merge.

## Filtered search and compaction

A filtered query costs one index probe **per segment**, so its latency tracks
the number of segments rather than the size of the corpus. That is what
compaction exists to bound, and the effect is the largest single performance
lever in the system.

At **10M spans**, uncompacted against default compaction:

| Metric | Uncompacted | Default compaction |
|---|---:|---:|
| Attribute filter p50 | 14.8 ms | 2.4 ms |
| Attribute filter p95 | 33.4 ms | 4.1 ms |
| Attribute filter p99 | 220 ms | 14.1 ms |
| Trace lookup p99 | 4.65 ms | 2.28 ms |

At **100M spans** (~55 GB on disk), all three columns from the same harness,
differing only in `--compaction-fanout` and `--compaction-max-segment-bytes`:

| 100M spans | Uncompacted | Default (256 MiB cap) | 1 GiB cap |
|---|---:|---:|---:|
| Attribute filter p50 | 155.5 ms | 9.8 ms | **2.3 ms** |
| Attribute filter p95 | 747.3 ms | 27.1 ms | **9.3 ms** |
| Attribute filter p99 | 1664.6 ms | 72.9 ms | **22.2 ms** |
| Trace lookup p99 | 7.72 ms | 1.82 ms | **0.99 ms** |
| Segments | ~10,100 † | ~380 † | ~100–125 ‡ |
| Peak RSS | 0.43 GB | 2.0 GB | 6.7 GB ‡ |
| Sustained ingest | 59,025/s | 40,894/s | 31,267/s |

† Extrapolated from mid-run samples, not measured directly; the benchmark
deletes its data directory on exit.
‡ Sampled directly during the run at 20-second intervals. The segment count
oscillates between about 97 and 125 over the last five minutes of ingest as
merges create and retire segments, and peak RSS is the maximum of those
samples — a shorter-lived merge spike between samples could exceed it.

Read honestly: compaction is worth roughly **16–28x** on filtered search at the
default cap, and raising the cap to 1 GiB is worth roughly another **3–4x** on
top. **At a 1 GiB cap, filtered-search p99 is 22.2 ms at 100M spans — inside
the 50 ms bar this project sets itself**, where the 256 MiB default measures
72.9 ms and misses it.

That win is paid for in memory and ingest: peak RSS 2.0 → 6.7 GB, and sustained
ingest a further 24% lower (40,894 → 31,267 spans/s). Raising the cap is the
right trade for a large store that is read more than written, and the wrong one
for a memory-constrained host. Both are one flag apart.

**These are single-run measurements on one machine at 100M spans. Nothing above
that size has been measured**, and segment count still grows with the corpus, so
the same tail returns at a large enough store. The structural answer remains a
per-segment inverted index, which is not built.

## Trace lookup

Limited queries decode only the records they return, which is why trace lookup
stays fast as the corpus grows: p99 1.270 ms at 1M
([`BENCHMARKS.md`](../../BENCHMARKS.md)), 2.28 ms at 10M and 1.82 ms at 100M
under default compaction. It is not *entirely* scale-independent — the 100M
uncompacted column shows 7.72 ms — because a lookup still probes each segment's
trace index, so it tracks segment count too, just far less steeply than a
filtered search.

## Sizing guidance

There is no single "spans per node" answer, because the binding constraint
moves with the workload. Work through it in this order:

1. **Disk.** Ingest a representative sample and read `bytes_on_disk`. Budget
   headroom for superseded versions (they persist until compaction rewrites
   their segment) and for a merge's output existing alongside its inputs.
2. **Filtered-search latency.** This is what degrades first as a store grows,
   and it tracks segment count. Watch `segment_count` and the
   `--compaction-max-segment-bytes` trade above.
3. **Memory.** Driven by the compaction cap, not the corpus. Pick the cap your
   host can hold during a merge.
4. **Ingest rate.** Only if you are approaching the ~212k spans/s design
   ceiling. Batch size is the first lever — the per-batch costs are paid once
   per request regardless of how many spans it carries.
5. **File descriptors.** One per segment. Fine with compaction on; a real
   limit with it off.

## What is not measured

Stated plainly, because absence of a number is not a claim of good behaviour:

- Anything above 100M spans on a single node.
- Query performance under concurrent read load — the query percentiles above
  were sampled without competing readers.
- Sustained mixed read/write workloads.
- Behaviour on spinning disks or network-attached storage; all measurements are
  on local SSD.
- Protobuf versus JSON ingest cost (being re-measured, see above).

If you need one of these, measure it. The benchmarks are in the repository
precisely so you can.
