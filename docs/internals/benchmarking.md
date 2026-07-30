# Benchmarking

Three benchmarks, three jobs. All build and drive the **real** release server
over its real HTTP path; none estimates anything.

| Binary | Answers | Writes |
|---|---|---|
| `bench` | "Is the canonical corpus still fast?" — ingest rate plus trace-lookup and filtered-query percentiles | [`BENCHMARKS.md`](../../BENCHMARKS.md) |
| `ingest-bench` | "Where does ingest throughput actually go?" — a matrix over protocol, keep-alive, concurrency, and durability | [`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md) |
| `storage-bench` | "How many bytes on disk per byte ingested, and what does that cost?" | [`STORAGE-BENCHMARK.md`](../../STORAGE-BENCHMARK.md) |

None is part of [`./ci.sh`](../../ci.sh). Run them when a change could
plausibly move performance or on-disk size.

## `bench` — the canonical corpus

```sh
cargo build --release
cargo run --release --bin bench
```

It starts `target/release/traza-server` on a free loopback port with a fresh
temporary data directory, ingests 1,000,000 spans over `POST /v1/spans` at
1,000 spans per request, then samples trace lookups and attribute-filtered
queries. It rewrites `BENCHMARKS.md` from its own measurements and deletes its
data directory on exit.

Three gates are reported PASS/FAIL rather than being substituted or estimated:
sustained ingest at or above 50,000 spans/s, trace-by-id p95 under 50 ms, and
attribute-filtered p95 under 300 ms.

JSON generation is deliberately **inside** the timed ingest loop, so the
reported rate includes client serialization and loopback HTTP overhead. That
makes it a floor on what the server can do, not a ceiling.

### Environment overrides

| Variable | Effect |
|---|---|
| `TRAZA_BENCH_SPANS` | Corpus size. **A non-default value does not rewrite `BENCHMARKS.md`** — it prints `(experimental corpus — BENCHMARKS.md not rewritten)`, so a scaling experiment cannot silently replace the published numbers |
| `TRAZA_BENCH_COMPACTION_FANOUT` | Passes `--compaction-fanout` to the server it spawns; `0` disables compaction |
| `TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES` | Passes `--compaction-max-segment-bytes`. Defaults to the real `CompactionConfig::default()` value rather than a literal, so the two cannot drift |

```sh
TRAZA_BENCH_SPANS=10000000 cargo run --release --bin bench   # scaling run, does not publish
```

## `ingest-bench` — the ingest matrix

```sh
cargo build --release
cargo run --release --bin ingest-bench -- --spans 1000000 --runs 5
```

| Flag | Meaning |
|---|---|
| `--spans N` | Spans per run |
| `--runs N` | Runs per scenario; the report is the **median**, with min and max shown |
| `--batch N` | Spans per request |
| `--concurrency N` | Client threads. Repeatable; default set is 1, 4, 8, 16 |
| `--only SUBSTRING` | Filter scenarios by name |

`TRAZA_BENCH_SERVER=/path/to/traza-server` points it at a different build, so a
before/after comparison runs through **one** client. Measuring two builds with
two clients would put the client's own change inside the difference being
attributed to the server.

It rewrites `INGEST-BENCHMARK.md`.

### What it refuses to report

A run is refused as a result, not reported as a number, if a batch failed, if
the stored record count came up short, or if the server shed a connection.
Every non-`buffered` run restarts the server and re-verifies the record count,
because the mode's whole claim is that acknowledgement survives restart.

This matters more than it sounds: a throughput number from a run that dropped
data is worse than no number.

## `storage-bench` — bytes in versus bytes kept

```sh
cargo build --release
cargo run --release --bin storage-bench
```

Three corpora, each into its own fresh server and data directory: `generic`
(service traces, the same span shape `bench` uses), `llm` (OpenLLMetry
attributes with ~2 KiB of prompt and completion text per span), and
`pinned-context` (a 320 KiB byte-identical context per call, above the payload
threshold, so content-addressed offloading engages).

"Ingested" is the sum of request-body lengths actually written to the socket,
not a declared corpus size. "On disk" is a recursive walk of the whole data
directory — segments, write-ahead log, payload store, and everything else — not
the `bytes_on_disk` field of `/v1/stats`, which counts segments only. Between
the two it forces a flush and then polls until the segment count and disk usage
stop moving, so compaction is never caught mid-rewrite holding both its inputs
and its output.

Everything runs at shipped defaults. A storage number measured under a tuned
configuration answers a question nobody asked.

| Variable | Effect |
|---|---|
| `TRAZA_STORAGE_BENCH_GENERIC_SPANS` | Corpus size for `generic` (default 1,000,000) |
| `TRAZA_STORAGE_BENCH_LLM_SPANS` | Corpus size for `llm` (default 200,000) |
| `TRAZA_STORAGE_BENCH_PINNED_CONTEXT_SPANS` | Corpus size for `pinned-context` (default 10,000, ~3.1 GiB ingested) |
| `TRAZA_STORAGE_BENCH_KEEP` | Leave each corpus's data directory on disk instead of deleting it, and print the paths. Set it when something downstream needs the real segment bytes — measuring how well they would compress, for instance, which has to run against the files this benchmark produced rather than a re-creation of them. **You are responsible for deleting them**; the `pinned-context` corpus alone is ~26 MiB and `llm` is ~905 MiB |

The ratio is reported the way a storage comparison expects to read it —
ingested : stored — **including when it is below 1:1 and therefore not a
compression ratio at all.** Traza does not compress segments, and on the first
two corpora it keeps roughly twice the bytes it was sent. Inverting that into a
flattering number is the specific dishonesty this binary exists to prevent.
[`docs/storage-comparison.md`](../storage-comparison.md) puts the output next to
OpenObserve's published Elasticsearch comparison.

## Reporting rules

These are project rules, not suggestions.

**Never state a performance number you have not measured.** Not an estimate,
not an extrapolation presented as a measurement, not a number carried over from
an older version. If you are unsure, say so instead of writing a confident
sentence.

**Never edit `BENCHMARKS.md` or `INGEST-BENCHMARK.md` by hand.** They are
generated. If your change affects performance, regenerate the relevant one and
include the run in your PR description.

**Cite the file, not the number.** Documentation should point at the
measurement record rather than copying figures into prose that then goes stale.
Where a number does appear in prose, it must be traceable to a committed
measurement file, and it must say what machine and corpus produced it.

**Label extrapolations as extrapolations.** `BENCHMARKS.md` and
`INGEST-BENCHMARK.md` already do this — for example marking segment counts
extrapolated from mid-run samples distinctly from counts sampled directly.
Follow that.

**Compare like with like.** The one substantive trap this project has already
hit: comparing two things that differ in more than the variable under test.
A protobuf-vs-JSON comparison that actually compares two different routes is
not a wire-format measurement. If you cannot isolate the variable, report the
comparison as unmeasured rather than reporting a number that means something
else.

**Report the shape, not just the ratio.** "Throughput was flat from 8 to 16
clients, and now it rises" says something a percentage does not: that a
contended lock was partially relieved rather than a constant factor improved.

**Name what is not measured.** The 100M-span compaction figures were measured
on one machine in a single run and are untested above that size; both
statements travel with the numbers.

## Reading a stage breakdown

`GET /v1/metrics` gives per-stage engine timings, and `INGEST-BENCHMARK.md`
shows how to use them. Two cautions:

- **Writer-lock wait is not a cost, it is a queue.** It measures contention for
  everything else. If it dominates, the fix is to do less work while holding
  the lock, not to make that work faster.
- **Stage percentiles are approximate by construction.** Latencies land in
  power-of-two nanosecond buckets, so a reported percentile is the upper bound
  of the bucket the true value falls in — at most 2x high, never low. They rank
  stages against each other. **They are not request latencies and must not be
  published as such.** The benchmarks measure end-to-end request latency
  exactly, from the client, with a plain `Instant`. See
  [monitoring](../operations/monitoring.md#how-accurate-the-percentiles-are).
