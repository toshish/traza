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

## Load conditions

**This machine is not a lab.** Sections are labelled with the conditions they
were taken under, because the same code measures very differently on a quiet
host and a busy one, and mixing the two silently is how a benchmark starts
lying.

- Sections marked **"on a quiet machine"** were taken with nothing else
  running. Treat those as levels.
- The [seal-off-the-lock comparison](#moving-the-seal-off-the-lock-what-it-bought)
  was taken with a 1-minute load average between 8.9 and 12.8 on 10 hardware
  threads, dominated by one unrelated process pinning a core for the whole
  window. Its **before and after builds were alternated round-robin**, so the
  ratio is sound and the absolute levels are depressed. Read the ratio.
- Anything reported as a gate against
  [the roadmap](docs/roadmap.md) says explicitly whether the host was idle.
  A target is not met on a contended machine, however good the median looks.

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
Every row is the MEDIAN of 5 runs, each on a fresh data directory. Scenarios are run ROUND-ROBIN rather than one at a time, and their order is ROTATED each round, so each scenario's repeats are spread across the whole wall-clock window and across positions within a round. Background load then hits all of them alike instead of landing on whichever ran during a spike or whichever is pinned to the same phase of a periodic load. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `e205232`
- Corpus: 1000000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS of each percentile. Read it with the load model in mind: this is a closed-loop generator with a fixed number of workers, all saturating, so latency includes queueing and by Little's law tracks concurrency divided by throughput. Latencies are therefore only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue behind.

| Scenario | Protocol | Route | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms | Bytes/span | Decode ns/span |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| direct-engine-wal | native-json | n/a | n/a | 8 | **202567** | 128602 | 213958 | 18.11 | 202.89 | 428.91 | n/a | n/a |
| direct-engine-buffered | native-json | n/a | n/a | 8 | **334702** | 185193 | 353729 | 0.21 | 56.81 | 623.16 | n/a | n/a |
| http-native-json-wal-keepalive-off | native-json | /v1/spans | false | 8 | **157545** | 120673 | 217733 | 34.01 | 176.10 | 378.70 | 256 | 714 |
| http-native-json-wal-keepalive-on | native-json | /v1/spans | true | 8 | **207645** | 151854 | 225870 | 17.66 | 191.44 | 412.13 | 256 | 743 |
| http-native-json-wal-c1 | native-json | /v1/spans | true | 1 | **47926** | 40608 | 81796 | 14.95 | 72.45 | 80.51 | 256 | 609 |
| http-otlp-json-wal-c1 | otlp-json | /v1/traces | true | 1 | **64203** | 42238 | 70343 | 10.96 | 47.78 | 61.42 | 455 | 2102 |
| http-otlp-protobuf-wal-c1 | otlp-protobuf | /v1/traces | true | 1 | **77103** | 43604 | 81645 | 9.01 | 44.14 | 51.05 | 152 | 630 |
| http-native-json-wal-c4 | native-json | /v1/spans | true | 4 | **187395** | 127571 | 200816 | 16.17 | 64.00 | 74.22 | 256 | 750 |
| http-otlp-json-wal-c4 | otlp-json | /v1/traces | true | 4 | **180789** | 109944 | 192028 | 16.38 | 66.81 | 85.37 | 455 | 2440 |
| http-otlp-protobuf-wal-c4 | otlp-protobuf | /v1/traces | true | 4 | **193587** | 120519 | 198759 | 15.72 | 62.82 | 74.26 | 152 | 735 |
| http-native-json-wal-c8 | native-json | /v1/spans | true | 8 | **208519** | 157533 | 221480 | 17.13 | 197.51 | 409.30 | 256 | 778 |
| http-otlp-json-wal-c8 | otlp-json | /v1/traces | true | 8 | **199955** | 145440 | 208080 | 19.59 | 201.02 | 415.94 | 455 | 2734 |
| http-otlp-protobuf-wal-c8 | otlp-protobuf | /v1/traces | true | 8 | **218717** | 191364 | 226584 | 17.37 | 186.37 | 394.14 | 152 | 726 |
| http-native-json-wal-c16 | native-json | /v1/spans | true | 16 | **212370** | 156388 | 221499 | 17.64 | 656.21 | 887.78 | 256 | 817 |
| http-otlp-json-wal-c16 | otlp-json | /v1/traces | true | 16 | **193385** | 150866 | 209335 | 19.87 | 712.12 | 982.01 | 455 | 2734 |
| http-otlp-protobuf-wal-c16 | otlp-protobuf | /v1/traces | true | 16 | **206929** | 164200 | 218873 | 18.08 | 671.82 | 927.86 | 152 | 797 |
| profile-throughput-c1 | native-json | /v1/spans | true | 1 | **60837** | 44823 | 80866 | 11.00 | 27.33 | 113.26 | 256 | 597 |
| profile-throughput-c4 | native-json | /v1/spans | true | 4 | **166732** | 93419 | 228729 | 14.08 | 44.52 | 146.74 | 256 | 749 |
| profile-throughput-c8 | native-json | /v1/spans | true | 8 | **211727** | 164940 | 257517 | 15.61 | 105.14 | 477.15 | 256 | 931 |
| profile-throughput-c16 | native-json | /v1/spans | true | 16 | **209488** | 177071 | 242172 | 19.92 | 204.10 | 1486.77 | 256 | 937 |
| profile-balanced-c1 | native-json | /v1/spans | true | 1 | **62656** | 33256 | 74976 | 9.05 | 47.47 | 93.32 | 256 | 632 |
| profile-balanced-c4 | native-json | /v1/spans | true | 4 | **170136** | 113634 | 173307 | 17.28 | 67.07 | 104.08 | 256 | 674 |
| profile-balanced-c8 | native-json | /v1/spans | true | 8 | **190608** | 145789 | 212150 | 18.94 | 196.61 | 417.39 | 256 | 756 |
| profile-balanced-c16 | native-json | /v1/spans | true | 16 | **189845** | 142757 | 200428 | 20.18 | 717.91 | 986.15 | 256 | 787 |
| profile-latency-c1 | native-json | /v1/spans | true | 1 | **59646** | 41874 | 65549 | 10.02 | 42.32 | 62.88 | 256 | 576 |
| profile-latency-c4 | native-json | /v1/spans | true | 4 | **165345** | 93958 | 176932 | 17.99 | 68.75 | 94.10 | 256 | 689 |
| profile-latency-c8 | native-json | /v1/spans | true | 8 | **163900** | 143421 | 175231 | 18.54 | 254.93 | 347.22 | 256 | 708 |
| profile-latency-c16 | native-json | /v1/spans | true | 16 | **169614** | 77994 | 182357 | 18.17 | 602.22 | 673.70 | 256 | 728 |
<!-- END GENERATED -->


Client-side JSON encoding of the corpus runs at ~800k spans/s, so the client
is not the constraint anywhere in this table.

## Against the roadmap target

**Target (§1.6): 250,000 spans/s sustained in `wal` mode. STILL NOT
CONFIRMED, and the most recent measurement cannot confirm it.**

The best figure remains 250,453 spans/s (`--profile throughput`, concurrency
16, median of 5 rotated rounds, min 122,768, max 261,215) taken at 0.16 on a
host whose 1-minute load average ranged 6.5 to 47.8. The medians cleared the
bar; the minima did not.

The 0.19 matrix above was taken while an unrelated process held a core for the
entire window, and it puts concurrency 16 **below** concurrency 8 at every
profile (`throughput` 209,780 against 221,499). That inversion is the
signature of oversubscription — 16 client threads plus server threads against
9 usable hardware threads — not of an engine limit, and it means **this run
says nothing about the gate one way or the other.** It is reported because
hiding it would be worse, not because it is evidence.

What the seal-off-the-lock work does change is the *reason* the gate is open.
The ceiling was the writer lock, held 88% of a run at the default setting with
sealing as three quarters of that. It is not that any more. The next thing to
measure, on an idle machine, is `wal_write` and `wal_fsync` — see
[the decomposition](#the-limiting-stage).

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

Server-side stage totals from `/v1/metrics`, one 1M-span run at c8. Measured
at `ddd185a`, the commit before sealing moved off the writer lock, on a machine
carrying background load (1-minute load average 7.9 to 13.6 on 10 hardware
threads, dominated by one unrelated process pinning a core):

| Stage | `--flush-spans` 5,000 | 10,000 (default) | 30,000 |
|---|---:|---:|---:|
| writer lock wait | 35,233 ms | 27,084 ms | 19,031 ms |
| **segment seal** | **5,180 ms** / 200 calls | **3,616 ms** / 100 | **2,446 ms** / 33 |
| wal encode | 2,880 ms | 2,853 ms | 2,858 ms |
| wal fsync | 2,213 ms | 1,939 ms | 1,336 ms |
| wal write | 1,003 ms | 1,099 ms | 596 ms |
| decode (wire → spans) | 850 ms | 780 ms | 834 ms |
| buffer upsert | 188 ms | 196 ms | 278 ms |
| **In-lock total** | **6,371 ms** | **4,911 ms** | **3,320 ms** |
| Wall clock | 6.59 s | 5.57 s | 4.09 s |
| **Lock held** | **97%** | **88%** | **81%** |
| **Sealing's share of in-lock** | **81%** | **74%** | **74%** |

Read it as follows. `writer lock wait` is not a cost, it is the **queue** for
everything else. The work actually performed while holding the writer lock is
`wal write + buffer upsert + segment seal`, and at the default setting that is
4.911 s against a 5.57 s wall clock. **The lock is saturated, and roughly three
quarters of what it is held for is sealing** — work that needs no lock at all.
`write_segment` converts spans to records, encodes the segment, writes it,
fsyncs, renames, fsyncs the directory, then reopens and parses the result, all
on a private vector no other thread can reach. Only the final push into the
segment list needs exclusion.

That decomposition was first taken on v0.17 and is reproduced here on
`ddd185a` **after** the WAL rework of PR #15, which added a second flush
trigger and 788 lines to `src/lib.rs`. The shares moved by a point or two; the
conclusion did not. What PR #15 did change is the *floor*: `wal fsync` is now a
comparable cost to sealing at the tighter thresholds, so relieving the lock
does not make sealing free, it makes fsync the next thing to look at.

Decode, by contrast, is ~800 ms of ~42 s of client thread-time — **about 2%**.
**That is why a 9.2x faster protobuf decoder is not a throughput result.** It
is a correct answer to which wire format is cheaper, and a real reduction in
per-span CPU. It is not a route to 250k spans/s, and nothing here should be
read as one.

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

## Moving the seal off the lock: what it bought

Sizing seals correctly amortizes the fixed cost; it does not remove it, and the
lock was still held ~81% of a run at `--flush-spans 30000`. Larger thresholds
keep paying in tail latency and in write-buffer memory, so that direction runs
out. **Sealing now happens with no engine lock held** — drain under a short
lock, write with nothing held, publish under a short lock, the shape PR #15
gave compaction and expiry.

### Before and after, round-robin

The host was not idle (see [Load conditions](#load-conditions)), so the two
builds were **alternated at invocation granularity** — before, after, before,
after — for four rounds of a 1M-span corpus at concurrency 8, `wal` durability,
1,000-span batches, compaction off. Drift in background load is shared between
them instead of landing on whichever ran second. Each cell is the median of
four rounds.

| `--profile` (`--flush-spans`) | Before | After | Change |
|---|---:|---:|---:|
| `throughput` (30,000) | 162,763 | **222,683** | **+37%** |
| `balanced` (10,000) | 116,612 | **176,004** | **+51%** |
| `latency` (5,000) | 83,400 | **180,331** | **+116%** |

Two things in that table matter more than the percentages.

**The profiles converge.** Before, `--flush-spans` spanned a 2x throughput
range (83k to 163k) because a smaller threshold meant more seals and every seal
stopped every ingesting thread. After, `latency` and `balanced` measure within
3% of each other. Sealing more often is no longer expensive, which is the
direct observable consequence of it not being on the critical path — and it is
the setting a latency-sensitive deployment actually wants.

**The contention signal collapses.** `writer lock wait` at `balanced` fell from
42,455 ms to 15,919 ms over the same 1,000 batches, ~62%. That is threads no
longer queueing behind a segment write.

### The decomposition afterwards, and where the lock went

`traza_segment_seal_locked` is the part of a seal that holds an engine lock —
the drain, the publish, and the buffer-and-log reconcile. It is sampled from
*after* each guard is acquired, so it is lock occupancy and not lock wait, and
it can therefore be summed against the other in-lock stages. Same host, 1M
spans at c8:

| In-lock stage | `latency` | `balanced` | `throughput` |
|---|---:|---:|---:|
| wal write | 4,145 ms | 4,304 ms | 3,186 ms |
| buffer upsert | 249 ms | 285 ms | 297 ms |
| **segment seal (lock held)** | **468 ms** | **585 ms** | **620 ms** |
| **In-lock total** | **4,861 ms** | **5,174 ms** | **4,102 ms** |
| **Sealing's share of in-lock** | **10%** | **11%** | **15%** |
| Seal wall time, all phases | 7,396 ms | 5,802 ms | 4,138 ms |
| Seals over 1M spans | 129 | 60 | 29 |

**Sealing went from 74-81% of in-lock work to 10-15%.** Only about a tenth of a
seal's wall time is now spent holding anything.

**The writer lock is still busy, and `wal write` is now what it is busy with** —
78-85% of the in-lock total. Two things pushed it up from the ~1,100 ms it
measured before the change: throughput rose, so the same 1,000 batches are
written in less wall time; and the segment write now competes with the log
write for the same device instead of being serialized behind the lock. So the
next thing to attack is the log device and the write path to it, not the
engine's locking. That is a different problem from the one this change solved,
and it should be measured on an idle machine before anyone acts on it.

Seal *count* falls (100 → 60 at `balanced`) while each seal covers more spans,
because the buffer keeps filling while the segment is being written. Under
saturation most batches find a seal already running and coalesce into it —
`traza_segment_seals_coalesced_total` reached 916 of 1,000 batches at
`balanced`. That is the design working, not a backlog.

Two caveats stated plainly. These absolute numbers are **lower than the
[quiet-machine figures](#the-engine-limit-measured-earlier-on-a-quiet-machine)
elsewhere in this file** because the host was carrying an unrelated 100%-CPU
process throughout; the *ratio* is what this measurement supports, not the
level. And `segment seal` mean time rises sharply after the change (about 35 ms
to about 145 ms at `balanced`) for two compounding reasons that are both
expected: a seal now covers more spans, because the buffer keeps filling while
the segment is written, and its wall clock now includes contending with the
ingest that no longer waits for it. Seal *count* falls correspondingly. Neither
is a regression; they are what "this work moved off the critical path" looks
like from a stage timer.

### The two constraints, corrected

An earlier attempt on v0.17 found two constraints and proposed fixes for both.
The constraints were real. **Both proposed fixes were wrong**, and the
correction is the interesting part.

1. **Segment ids must be claimed when the buffer is drained, not when the write
   finishes.** Segment path order *is* recency order here, so two seals
   finishing out of order would silently invert last-write-wins. This one
   stands, and `merge_tail_run` already did it for the same reason. What was
   missed is the *other* half: compaction claims ids from the same counter, so
   a merge that claims an id while a seal holds a lower unpublished one would
   sort its (strictly older) output above that seal's segment. Compaction now
   declines while any seal is unpublished.

2. **Spans must stay visible while being sealed.** Taking them out of the write
   buffer before the segment lands leaves already-acknowledged spans in neither
   the buffer nor a segment — briefly invisible to readers, violating the
   invariant `get_trace` documents. Also true. But the proposed fix — a third
   reader-visible "sealing" tier — was unnecessary. **The merge never removes
   data from visibility**: its inputs stay live and pinned until the output is
   published, then swap. A seal does the same. The spans stay in the write
   buffer for the whole write and are evicted only after the segment is
   published, so no reader ever consults a third place and no precedence rule
   changes.

   That leaves one real subtlety, and it is where content-based identity would
   have destroyed data: a span re-ingested *during* the seal is a newer version
   sitting in the buffer while the segment holds the older one. The buffer
   outranks segments, so reads are already correct — but the post-publish
   eviction must drop only keys whose current buffer value is still the one
   that was sealed. Comparing values cannot answer that: a span re-ingested
   unchanged is a newer version that happens to look identical. The write
   buffer therefore holds `Arc<Span>` and the eviction test is handle identity.
   That also makes the drain a pointer copy rather than a deep copy of ten
   thousand spans under the lock, which is the cost the change exists to avoid.

The **WAL rotation scheme** the earlier attempt designed was also dropped.
`Wal::rewrite` (PR #15) already stages and renames a log containing exactly a
given set of spans, and is crash-tested. But rewriting to the survivors on
*every* seal puts a re-serialization of everything admitted during the write —
thousands of spans at these rates — straight back under the writer lock, which
is most of what the change just bought. Reclamation therefore happens on the
bound that exists to bound the log, `--flush-wal-bytes` (64 MiB by default),
and amortizes over every seal since the last reclaim. A seal that empties the
buffer still discards the whole log, so an idle store behaves exactly as
before. Leaving records in the log is always safe: replaying a span a segment
already holds upserts it to the same value.

### What it cost

**Ingest is no longer throttled by sealing**, which was previously free
backpressure: no batch could be admitted while a seal ran. Past four times
`--flush-spans` of buffered records, an ingesting thread now waits for the seal
permit rather than letting its seal coalesce, so the write buffer stays
bounded. Below that bound, peak buffer memory is higher than before by roughly
one seal's worth of arrivals.

**`--flush-wal-bytes` became load-bearing.** Under sustained ingest the log now
runs up to that bound between reclamations instead of being emptied on every
seal, so restart replay is bounded by the setting rather than by one seal
interval. That is what the setting has always documented; it is now the thing
actually doing it.

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
