# Benchmarking

Four benchmarks, four jobs. All build and drive the **real** release server
over its real HTTP path; none estimates anything.

| Binary | Answers | Writes |
|---|---|---|
| `bench` | "Is the canonical corpus still fast?" — ingest rate plus trace-lookup and filtered-query percentiles | [`canonical-corpus.md`](../benchmarks/canonical-corpus.md) |
| `ingest-bench` | "Where does ingest throughput actually go?" — a matrix over protocol, keep-alive, concurrency, and durability | [`ingest.md`](../benchmarks/ingest.md) |
| `storage-bench` | "How many bytes on disk per byte ingested, and what does that cost?" | [`storage.md`](../benchmarks/storage.md) |
| `query-bench` | "What does a dashboard's aggregation cost — cold, and while the store is being written to?" | [`query.md`](../benchmarks/query.md) |

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
queries. It rewrites `canonical-corpus.md` from its own measurements and
deletes its
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
| `TRAZA_BENCH_SPANS` | Corpus size. **A non-default value does not rewrite `canonical-corpus.md`** — it prints `(experimental corpus — canonical-corpus.md not rewritten)`, so a scaling experiment cannot silently replace the published numbers |
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

It rewrites `ingest.md`.

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

**Never edit `canonical-corpus.md` or `ingest.md` by hand.** They are
generated. If your change affects performance, regenerate the relevant one and
include the run in your PR description.

**Cite the file, not the number.** Documentation should point at the
measurement record rather than copying figures into prose that then goes stale.
Where a number does appear in prose, it must be traceable to a committed
measurement file, and it must say what machine and corpus produced it.

**Label extrapolations as extrapolations.** `canonical-corpus.md` and
`ingest.md` already do this — for example marking segment counts
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

`GET /v1/metrics` gives per-stage engine timings, and `ingest.md`
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

## `query-bench` — aggregation, cold and under load

```sh
cargo build --release
cargo run --release --bin query-bench
```

Measures `GET /v1/stats/llm` and `GET /v1/sessions`: the endpoints a dashboard
actually calls, and the two things about them the other three benchmarks
cannot see.

**Cold.** Aggregates are served from a per-segment rollup that is cached in
process memory, so the first one after a restart is a different query from
every one after it. The only honest way to produce that is to restart the
server, which this does before EVERY measured shape — measuring two cold
queries against one restart would report the second as cold when the first had
already warmed it.

**Under concurrent ingest.** A segment fully inside a query's time window is
answered from its rollup; one that straddles the boundary is decoded.
Concurrent clients interleave their timestamps across segments, so with enough
of them no segment is fully inside any window and every query takes the slow
path. `TRAZA_QUERY_BENCH_THREADS` is therefore a first-class axis, not a
detail: a single-threaded ingest reports the easy case and never sees this.

A separate probe queries continuously for the whole of ingest, flush and
settle, which is the only window in which compaction has anything to do — a
settled store cannot show what a merge costs the next aggregation, because by
then every rollup has been rebuilt once and stays warm.

Every measured shape is checked for a non-empty answer, and the whole-corpus
shapes for the RIGHT answer: the rows must sum to the corpus size. A benchmark
that cannot tell a fast query from an empty one is measuring the HTTP stack,
and a supersede bug would show up here as a sum above the corpus rather than
as a suspiciously good latency.

| Variable | Effect |
|---|---|
| `TRAZA_QUERY_BENCH_SPANS` | Corpus size (default 500,000) |
| `TRAZA_QUERY_BENCH_THREADS` | Concurrent ingest clients (default 8) — the axis that decides whether any segment is fully inside a window |
| `TRAZA_QUERY_BENCH_COMPACTION_FANOUT` | Compaction fan-out; `0` disables it. Pinned rather than left to its defaults because it sets the segment count, and an unpinned run can measure a four-segment store against a seventy-segment one without saying so |
| `TRAZA_QUERY_BENCH_COMPACTION_MAX_SEGMENT_BYTES` | Size ceiling for compacted segments |
