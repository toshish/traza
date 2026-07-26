# Traza Ingest Benchmark

Measured by `cargo run --release --bin ingest-bench`. Every figure is the
**median of 5 runs**, each on a fresh data directory, with min and max shown
so the spread is visible rather than implied.

- **Machine:** macOS/aarch64, Apple M1 Max, 10 hardware threads
- **Corpus:** 1,000,000 spans per run, 1,000 spans per batch
- **Durability:** `wal` (the default) unless stated — an acknowledged write is
  fsynced and recoverable
- **Compaction:** disabled during ingest runs; it is a read-path optimization
  whose merges would otherwise steal CPU from the measurement
- **Cache:** cold data directory per run, created and deleted around each
- **Payloads:** generated before the clock starts, so these are server rates.
  Client encoding is reported separately and is not folded in.

Runs are refused as results, not reported as numbers, if a batch failed, if
the stored record count came up short, or if the server shed a connection.
Every non-`buffered` run restarts the server and re-verifies the record count,
because the mode's whole claim is that acknowledgement survives restart.

## Protocol: what a wire format actually costs

### The comparison that was wrong

An earlier revision of this file claimed **"protobuf is slower than JSON at
every concurrency"**, citing 65,480 vs 82,805 spans/s at concurrency 1. That
number could not support that claim, because the two figures came from
**different routes**:

- the JSON figure posted to `/v1/spans`, the native span route, which
  deserializes straight to `Vec<Span>` and performs no OTLP mapping at all;
- the protobuf figure posted to `/v1/traces`, the OTLP route, which decodes
  the wire format **and** applies the whole OTLP semantic mapping — resource
  and scope resolution, `service.name` extraction, typed `AnyValue`
  flattening, status and link mapping.

Every "protobuf vs JSON" delta in that comparison therefore also contained the
entire OTLP mapping. It measured *route*, not *wire format*.

The benchmark now carries a third protocol, **`otlp-json`**, which posts
OTLP/HTTP JSON to `/v1/traces`. That holds the route and the mapping fixed, so:

- **`otlp-json` vs `otlp-protobuf` isolates the wire format** — same route,
  same mapping, same spans, same order.
- **`native-json` vs `otlp-json` isolates the OTLP route's mapping cost** —
  same wire format, different route.

Scenario labels now name the route (`http-native-json-…`, `http-otlp-json-…`,
`http-otlp-protobuf-…`); `http-json-wal` silently meaning "native route" is
part of how the wrong conclusion got made.

Two corpus bugs were fixed in the same pass, both of which flattered JSON:

- The protobuf encoder wrote the status code as **field 2**, which is
  `Status.message` and a different wire type. The decoder skipped it as an
  unknown field, so every protobuf span in the corpus arrived with **no
  status** while both JSON corpora carried one.
- Service varied **round-robin per span**, which OTLP can only encode as one
  `ResourceSpans` per span — the pathological shape. Service now varies in
  contiguous runs of 50, so all three encodings carry the same spans in the
  same order with a realistic resource grouping.

### Decode cost, before and after

Decode is measured by the server's own `traza_http_decode_ns_sum` /
`traza_http_decoded_spans_total` counters, scraped from `/v1/metrics` after
each run. On `/v1/traces` that covers the wire decode **and** the OTLP-to-Span
mapping; on `/v1/spans` it covers the `serde` deserialization. It is the only
figure here that isolates the protocol question from the engine.

Both OTLP paths originally went through a `serde_json::Value` DOM —
`otlp_pb::traces_request_to_json` built one for protobuf,
`serde_json::from_slice` built one for JSON, and `otlp::spans_from_request`
walked it in both cases. **Both** have been lowered straight to `Span`, so
what follows compares optimized against optimized rather than an optimized
path against an unoptimized one.

Median ns/span over 5 runs of 1M spans, at concurrency 1:

| Path | Current | Optimized | Change |
|---|---:|---:|---:|
| OTLP protobuf → `Span` (`/v1/traces`) | 4,384 | **479** | **9.2x faster** |
| OTLP JSON → `Span` (`/v1/traces`) | 2,377 | **1,275** | **1.9x faster** |
| native JSON → `Span` (`/v1/spans`) | 626 | 650 | *control, untouched* |

The same shape holds at every concurrency, which is what makes it a result
rather than a fluctuation:

| Path | c1 cur | c1 opt | c8 cur | c8 opt | c16 cur | c16 opt |
|---|---:|---:|---:|---:|---:|---:|
| OTLP protobuf | 4,384 | 479 | 5,674 | 701 | 6,000 | 721 |
| OTLP JSON | 2,377 | 1,275 | 3,190 | 1,620 | 3,389 | 1,635 |
| native JSON (control) | 626 | 650 | 831 | 887 | 803 | 827 |

**The native-json row is the noise floor.** Its code is byte-identical in both
builds, so its drift between the two measurement sessions — +3% to +7% — is
what "no change" looks like here. The protobuf and JSON improvements are far
outside it.

### The verdict

**The original claim was wrong, and it is now wrong in the opposite
direction.**

- **As the code stood, on the same route,** protobuf decode *was* slower than
  JSON decode: 4,384 vs 2,377 ns/span, 1.8x. So the claim's *direction*
  happened to survive the controlled comparison — but the evidence offered for
  it was not evidence, and the stated cause was wrong. The roadmap attributed
  it to protobuf "decoding through a `serde_json::Value` DOM that the JSON
  route no longer uses"; OTLP JSON went through **the same DOM**. What
  actually separated them was protobuf's own decoder: a `format!("{byte:02x}")`
  per BYTE of every trace and span id — 24 million formatter calls and 24
  million throwaway `String`s per 1M-span batch — plus a `Map` and a `String`
  key for every `KeyValue` and `AnyValue`, built only for the mapper to take
  apart one call later.
- **With both paths optimized, protobuf is decisively faster:** 479 vs 1,275
  ns/span at c1, **2.7x**; 2.3x at both c8 and c16. Protobuf was never slower
  *as a wire format*. It was slower as one implementation of one.
- Optimized protobuf (479 ns/span) is now **faster than the native JSON route**
  (650 ns/span) *while also doing the full OTLP semantic mapping*. A binary
  format with length-prefixed fields, raw-byte ids and no escape handling pays
  for its own mapping and has change left over.
- **The OTLP mapping itself costs ~625 ns/span** at c1 (optimized `otlp-json`
  1,275 minus `native-json` 650), against payloads 33% larger. That is the
  price of the OTLP envelope and semantics at a fixed wire format.

### Payload size

Bytes on the wire per span, for the identical logical corpus:

| Protocol | Bytes/span | vs OTLP JSON |
|---|---:|---:|
| `otlp-protobuf` | **117** | **0.34x** |
| `native-json` | 257 | 0.75x |
| `otlp-json` | 342 | 1.00x |

Protobuf's payload is **2.9x smaller** than the same request in OTLP JSON,
which is the ratio a sound encoder should produce. In particular it is not
emitting one `ResourceSpans` per span — that would have shown up here as bloat
rather than compression. The benchmark's hand-written protobuf encoder is
representative.

### End-to-end throughput: measured, but do not read much into it

| Scenario | Protocol | Route | Concurrency | Median spans/s | Min | Max | Bytes/span | Decode ns/span |
|---|---|---|---:|---:|---:|---:|---:|---:|
| http-native-json-wal-c1 | native-json | /v1/spans | 1 | **58,355** | 55,842 | 63,963 | 257 | 650 |
| http-otlp-json-wal-c1 | otlp-json | /v1/traces | 1 | **78,977** | 55,787 | 80,270 | 342 | 1,275 |
| http-otlp-protobuf-wal-c1 | otlp-protobuf | /v1/traces | 1 | **71,574** | 51,859 | 80,073 | 117 | 479 |
| http-native-json-wal-c8 | native-json | /v1/spans | 8 | **121,716** | 89,493 | 177,943 | 257 | 887 |
| http-otlp-json-wal-c8 | otlp-json | /v1/traces | 8 | **137,742** | 126,690 | 146,650 | 342 | 1,620 |
| http-otlp-protobuf-wal-c8 | otlp-protobuf | /v1/traces | 8 | **134,368** | 125,964 | 154,080 | 117 | 701 |
| http-native-json-wal-c16 | native-json | /v1/spans | 16 | **131,817** | 128,151 | 140,478 | 257 | 827 |
| http-otlp-json-wal-c16 | otlp-json | /v1/traces | 16 | **142,181** | 138,745 | 145,932 | 342 | 1,635 |
| http-otlp-protobuf-wal-c16 | otlp-protobuf | /v1/traces | 16 | **117,446** | 112,217 | 136,833 | 117 | 721 |

**These absolute rates were measured on a CONTENDED machine** — 1-minute load
average 7–14 against 10 hardware threads, with a virtual machine, a
storage-indexing job and unrelated compute running. They are **not comparable
to the quiet-machine figures in the next section**, and the spread shows it:
`http-native-json-wal-c8` ranges from 89,493 to 177,943 spans/s across its five
runs, a 2x span on code that did not change.

At this noise level end-to-end throughput **cannot** resolve a decode
improvement, because decode is ~2% of the cost. Read the decode counters for
the protocol question and these rates for nothing finer than an order of
magnitude.

The one end-to-end movement clearly larger than the drift is protobuf at
**concurrency 1** — 46,465 → 71,574 spans/s — and that is where it should
appear: with a single client there is no writer-lock queue to hide behind, so
the 3.9 µs/span of decode that went away (3.9 s per 1M spans, against a
21.5 s → 14.0 s change in run time) lands directly on the critical path. At c8
and c16 the lock reabsorbs it, exactly as the 1.9%-of-cost figure predicts.

## The engine limit (measured earlier, on a quiet machine)

The figures in this section were taken at commit `1d89d11` on an idle machine.
They are unaffected by the protocol work, which touches only decode.

| Concurrency | Before (`7d36b83`) | After | Change |
|---|---:|---:|---:|
| 1 | 73,798 | 82,805 | +12% |
| 8 | 108,989 | 193,188 | +77% |
| 16 | 108,881 | **208,973** | **+92%** |

The shape matters more than the ratio: **before, throughput was flat from 8 to
16 clients** (108,989 → 108,881) — adding clients bought nothing. After, it
still rises. That is a contended lock being partially relieved, not a
constant-factor speedup.

## Full matrix

The block below is written by the benchmark itself; everything outside the
markers is analysis and survives a re-run.

<!-- BEGIN GENERATED -->
Every row is the MEDIAN of 3 runs, each on a fresh data directory. Scenarios are run ROUND-ROBIN rather than one at a time, and their order is ROTATED each round, so each scenario's repeats are spread across the whole wall-clock window and across positions within a round. Background load then hits all of them alike instead of landing on whichever ran during a spike or whichever is pinned to the same phase of a periodic load. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `ddd185a`
- Corpus: 1000000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS of each percentile. Read it with the load model in mind: this is a closed-loop generator with a fixed number of workers, all saturating, so latency includes queueing and by Little's law tracks concurrency divided by throughput. Latencies are therefore only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue behind.

| Scenario | Protocol | Route | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms | Bytes/span | Decode ns/span |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| profile-throughput-c8 | native-json | /v1/spans | true | 8 | **243792** | 236951 | 244474 | 12.07 | 94.00 | 108.04 | 256 | 834 |
| profile-balanced-c8 | native-json | /v1/spans | true | 8 | **192896** | 179467 | 195099 | 45.92 | 54.21 | 60.93 | 256 | 780 |
| profile-latency-c8 | native-json | /v1/spans | true | 8 | **159949** | 151787 | 161023 | 51.17 | 64.82 | 73.95 | 256 | 850 |
<!-- END GENERATED -->


Client-side JSON encoding of the corpus runs at ~800k spans/s, so the client
is not the constraint anywhere in this table.

## Against the roadmap target

**Target (§1.5): 250,000 spans/s sustained in `wal` mode. Best measured:
250,453 (`--profile throughput`, concurrency 16, median of 5 rotated rounds,
min 122,768, max 261,215). The median clears the bar; the spread straddles it,
and the host was shared during measurement — see
[the correction below](#correction-the-ceiling-is-a-setting-not-a-property-of-the-design)
before treating the gate as closed.** At the default `balanced` settings the
same run measures 197,056, so most of what used to look like a 16% design gap
was one default.

The roadmap attributes the target to "keep-alive + protobuf". Measurement
supports neither attribution:

- **Protobuf is not the lever, and it is not slow.** The controlled
  comparison is in [Protocol](#protocol-what-a-wire-format-actually-costs)
  above: with the route held fixed and both paths optimized, OTLP protobuf
  decodes 2.3-2.7x FASTER than OTLP JSON. It cannot be what unlocks the target
  either way, because decode is ~1.9% of ingest cost. (An earlier revision of
  this section claimed the opposite, from a benchmark that compared two
  different routes.)
- **Keep-alive is worth +11% at batch=20** (10,221 vs 9,203 spans/s) and
  nothing at batch=1000, where it measured slightly *negative*. At 245 KB and
  ~5 ms of server work per request, a ~50 µs connect is under 1% of the cost.

## The limiting stage

`direct-engine-wal` (193,637) and the native HTTP route at the same
concurrency (193,188) are the same number. **Removing HTTP entirely — no
socket, no parsing, no protocol — changes nothing.** Whatever limits ingest is
inside the engine, which is why neither wire format nor connection work moves
it.

Server-side stage totals from `/v1/metrics`, one 1M-span run at c8 (8 client
threads, ~42 s of client thread-time against a 5.32 s wall clock):

| Stage | Total | Calls | Mean |
|---|---:|---:|---:|
| writer lock wait | 25,970 ms | 1,000 | 25.97 ms |
| **segment seal** | **3,473 ms** | 100 | 34.74 ms |
| wal encode | 2,806 ms | 1,000 | 2.81 ms |
| wal fsync | 1,871 ms | 363 | 5.15 ms |
| wal write | 1,037 ms | 1,000 | 1.04 ms |
| decode (wire → spans) | 811 ms | 1,000 | 0.81 ms |
| buffer upsert | 196 ms | 1,000 | 0.20 ms |

Group commit amortized 900 acknowledgements across 363 fsyncs (2.5x).

Read it as follows. `writer lock wait` is not a cost, it is the **queue** for
everything else. The work actually performed while holding the writer lock is
`wal write + buffer upsert + segment seal` = **4.706 s against a 5.32 s wall
clock — the lock is held ~88% of the run.** It is saturated.

**Of the time spent holding the lock, segment sealing is 74%** — and sealing
needs no lock at all. `write_segment` converts spans to records, encodes the
segment, writes it, fsyncs, renames, fsyncs the directory, then reopens and
parses the result, all on a private vector no other thread can reach. Only the
final push into the segment list needs exclusion.

Decode, by contrast, was 811 ms of ~42 s of client time — **1.9%** — before
any of the protocol work above. **That is why a 9.2x faster protobuf decoder
is not a throughput result.** It is a correct answer to which wire format is
cheaper, and a real reduction in per-span CPU. It is not a route to 250k
spans/s, and nothing here should be read as one.

## Against the roadmap target

**Target (§1.5): 250,000 spans/s sustained in `wal` mode. Best measured:
208,973. NOT MET — 16% short.**

The roadmap once attributed the target to "keep-alive + protobuf".
Measurement supports neither attribution, though the protobuf half needs
restating rather than repeating:

- **Protobuf is the cheaper wire format to decode** — 2.3–2.7x cheaper than
  OTLP JSON on the same route, once both decoders are written properly, and
  2.9x smaller on the wire. But **decode is ~2% of ingest cost**, so choosing
  it does not move the gate. Prefer it for bandwidth and CPU, not throughput.
- **Keep-alive is worth +11% at batch=20** (10,221 vs 9,203 spans/s) and
  nothing at batch=1000, where it measured slightly *negative*. At 245 KB and
  ~5 ms of server work per request, a ~50 µs connect is under 1% of the cost.

The real limit is the **writer lock, held ~88% of a run, of which 74% is
segment sealing** — work that needs no lock.

## Correction: the ceiling is a setting, not a property of the design

An earlier version of this document read the decomposition above as *"a hard
ceiling near 212,000 spans/s on the current design, which no amount of client
concurrency passes"*. **That was wrong**, and the error is worth naming
precisely because the arithmetic was right.

The arithmetic was performed at one value of `--flush-spans`: 10,000, the
default. A segment seal has a **fixed cost per seal** — two fsyncs, a create
and a rename, and a reopen-and-parse of the finished segment — on top of its
per-span cost. Sealing less often amortizes that fixed part over more spans, so
the in-lock total falls and the ceiling moves. The ceiling is a function of the
setting.

Measured, same machine, same 1M-span corpus at concurrency 8, one round each:

| | `--flush-spans 10000` (`balanced`) | `--flush-spans 30000` (`throughput`) |
|---|---:|---:|
| Seals | 100 | 33 |
| Mean seal | 34.468 ms | 74.627 ms |
| **Segment seal total** | **3,447 ms** | **2,463 ms** |
| wal write | 968 ms | 635 ms |
| buffer upsert | 166 ms | 238 ms |
| **In-lock total** | **4,581 ms** | **3,336 ms** |
| Wall clock | 5.20 s | 4.15 s |
| Lock held | 88% | 80% |
| **Implied ceiling** | **218,300 spans/s** | **299,800 spans/s** |

The `balanced` column reproduces the original figure (4.581 s here against
4.706 s then, ~218k against ~212k), so the earlier measurement was sound — only
its generalization was not.

Tripling the spans per seal did **not** triple the mean seal time: it rose
2.17x, not 3x. Solving those two measured points for a fixed and a per-span
component gives roughly **14.4 ms of fixed cost per seal plus ~2.0 µs per
span** — derived from the two measurements above, not independently measured.
At 10,000 spans the fixed part is ~42% of every seal; at 30,000 it is ~19%.
That is the whole mechanism.

Group commit also improves with the profile's commit window: 2.5x amortization
(900 acks / 362 fsyncs) at `balanced` against 3.6x (967 acks / 267 fsyncs) at
`throughput`.

**Against the 250k target, measured rather than derived:** `--profile
throughput` at concurrency 16 measured a **median of 250,453 spans/s** over 5
rotated rounds (min 122,768, max 261,215) against `balanced`'s 197,056
(min 114,849, max 205,010). A separate 5-round run of the same configuration
measured 261,782 (min 213,217, max 269,465) at c16.

Read that spread honestly. The medians clear 250k; the minima do not, and the
range is wide because the host was shared during measurement (1-minute load
average ranged 6.5 to 47.8, mean 15.4, sampled every 15 s). **The target should
not be considered closed until it is re-measured on an idle machine.** What the
measurement does establish is that the gap was never 16% of "the current
design" — most of it was one default.

## What moving the seal off the lock would still buy

Sizing seals correctly amortizes the fixed cost; it does not remove it, and the
lock is still held ~80% of a run at `--flush-spans 30000`. Larger thresholds
keep paying in tail latency (p99 rises from 64.00 ms at 10,000 to 98.22 ms at
30,000 under a fixed 60k spans/s arrival rate) and in write-buffer memory, so
this direction runs out. Moving sealing off the lock is what removes the
constraint rather than repricing it.

Two constraints make that more than a local change — both found by attempting
it:

1. **Segment ids must be assigned when the buffer is drained, not when the
   write finishes.** Segment path order *is* recency order in this engine, so
   two overlapping seals finishing out of order would silently invert
   last-write-wins.
2. **Spans must stay visible while being sealed.** Taking them out of the
   write buffer before the segment lands leaves already-acknowledged spans in
   neither the buffer nor a segment — briefly invisible to readers, violating
   the invariant `get_trace` documents. The existing
   `reads_never_miss_committed_spans` test does *not* catch this: its writer is
   single-threaded and `flush()` is synchronous end to end, so the window
   never opens.

The fix is a third "sealing" tier that readers consult between the segments
and the write buffer (only three sites read the buffer directly), plus a WAL
that rotates its active log aside at drain time instead of truncating it —
truncation would discard records appended during an unlocked seal. Recovery
then replays rotated logs in order before the active one. It needs a
concurrent-ingest-during-seal test to be worth trusting.

This was implemented far enough to find those two constraints, then reverted
rather than merged: the visibility regression is precisely the kind the
current suite would pass over.

## Reproducing

```
cargo build --release
cargo run --release --bin ingest-bench -- --spans 1000000 --runs 5
```

`--only SUBSTRING` filters scenarios (`--only wal-c` gives the three-protocol
matrix), `--batch N` changes batch size, and
`TRAZA_BENCH_SERVER=/path/to/traza-server` measures a different build through
this same client — which is how the before/after decode table above was taken,
so the client's own changes are not inside the difference being attributed to
the server.

Check `uptime` before believing any absolute rate from this harness. The
decode counters tolerate a busy machine; the end-to-end rates do not.
