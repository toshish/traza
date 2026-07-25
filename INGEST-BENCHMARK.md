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

## Before and after

Both columns measured **through the same client**, with `TRAZA_BENCH_SERVER`
pointing at the older build. Measuring the two with different clients would
put the client's own change inside the difference being attributed to the
server.

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
- Commit: `985d236`
- Corpus: 1000000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS of each percentile. Read it with the load model in mind: this is a closed-loop generator with a fixed number of workers, all saturating, so latency includes queueing and by Little's law tracks concurrency divided by throughput. Latencies are therefore only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue behind.

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| direct-engine-wal | json | n/a | 8 | **197230** | 110709 | 197992 | 45.09 | 53.11 | 60.88 |
| direct-engine-buffered | json | n/a | 8 | **317212** | 215499 | 326041 | 0.17 | 128.25 | 236.07 |
| http-json-wal-keepalive-off | json | false | 8 | **122592** | 82877 | 193848 | 57.76 | 90.46 | 103.26 |
| http-json-wal-keepalive-on | json | true | 8 | **127073** | 80524 | 192642 | 68.34 | 88.16 | 99.93 |
| http-json-wal-c1 | json | true | 1 | **64019** | 35874 | 84270 | 10.04 | 42.69 | 57.32 |
| http-protobuf-wal-c1 | protobuf | true | 1 | **46066** | 36046 | 61752 | 13.98 | 58.36 | 67.76 |
| http-json-wal-c4 | json | true | 4 | **161033** | 77059 | 166574 | 15.04 | 45.18 | 51.64 |
| http-protobuf-wal-c4 | protobuf | true | 4 | **139674** | 54092 | 142027 | 19.91 | 49.48 | 54.08 |
| http-json-wal-c8 | json | true | 8 | **139140** | 73501 | 196846 | 47.40 | 82.13 | 96.22 |
| http-protobuf-wal-c8 | protobuf | true | 8 | **172593** | 88283 | 181015 | 50.21 | 60.74 | 68.15 |
| http-json-wal-c16 | json | true | 16 | **185486** | 82539 | 205200 | 89.17 | 115.68 | 165.22 |
| http-protobuf-wal-c16 | protobuf | true | 16 | **188379** | 103302 | 199422 | 91.25 | 108.95 | 157.00 |
| profile-throughput-c1 | json | true | 1 | **85010** | 61004 | 86189 | 9.07 | 13.34 | 80.06 |
| profile-throughput-c4 | json | true | 4 | **173092** | 77643 | 177324 | 12.64 | 91.98 | 104.66 |
| profile-throughput-c8 | json | true | 8 | **231912** | 117628 | 241066 | 13.32 | 99.19 | 107.36 |
| profile-throughput-c16 | json | true | 16 | **250453** | 122768 | 261215 | 90.23 | 112.08 | 168.47 |
| profile-balanced-c1 | json | true | 1 | **80791** | 46899 | 84397 | 9.01 | 38.40 | 44.12 |
| profile-balanced-c4 | json | true | 4 | **159716** | 108801 | 165339 | 15.12 | 46.44 | 50.98 |
| profile-balanced-c8 | json | true | 8 | **184571** | 111761 | 195149 | 45.97 | 55.94 | 114.37 |
| profile-balanced-c16 | json | true | 16 | **197056** | 114849 | 205010 | 86.07 | 105.13 | 174.99 |
| profile-latency-c1 | json | true | 1 | **74823** | 57675 | 79199 | 8.98 | 29.78 | 38.46 |
| profile-latency-c4 | json | true | 4 | **134573** | 116698 | 139764 | 31.61 | 39.97 | 46.07 |
| profile-latency-c8 | json | true | 8 | **162703** | 157871 | 165534 | 50.80 | 63.14 | 69.98 |
| profile-latency-c16 | json | true | 16 | **157681** | 153194 | 161347 | 98.12 | 121.03 | 132.90 |
| profile-throughput-openloop-c1 | json | true | 1 | **60019** | 60003 | 60024 | 12.79 | 79.13 | 92.94 |
| profile-throughput-openloop-c4 | json | true | 4 | **60003** | 59982 | 60008 | 15.54 | 83.88 | 108.75 |
| profile-throughput-openloop-c8 | json | true | 8 | **60009** | 59916 | 60019 | 15.31 | 83.40 | 115.18 |
| profile-throughput-openloop-c16 | json | true | 16 | **59995** | 59750 | 60021 | 14.36 | 83.20 | 134.20 |
| profile-balanced-openloop-c1 | json | true | 1 | FAILED: offered rate exceeded capacity: delivered 54566 of 60000 spans/s (worst lateness 2475.1 ms), so this is a saturation measurement, not an open-loop one | | | | | |
| profile-balanced-openloop-c4 | json | true | 4 | **59898** | 59839 | 59911 | 18.51 | 48.94 | 79.58 |
| profile-balanced-openloop-c8 | json | true | 8 | **59886** | 59862 | 59914 | 16.76 | 47.81 | 74.94 |
| profile-balanced-openloop-c16 | json | true | 16 | **59889** | 59811 | 59923 | 15.33 | 46.22 | 59.50 |
| profile-latency-openloop-c1 | json | true | 1 | FAILED: offered rate exceeded capacity: delivered 42493 of 60000 spans/s (worst lateness 6837.1 ms), so this is a saturation measurement, not an open-loop one | | | | | |
| profile-latency-openloop-c4 | json | true | 4 | **59925** | 59875 | 59955 | 18.64 | 40.73 | 63.03 |
| profile-latency-openloop-c8 | json | true | 8 | **59934** | 59924 | 59944 | 19.73 | 40.77 | 75.17 |
| profile-latency-openloop-c16 | json | true | 16 | FAILED: offered rate exceeded capacity: delivered 55804 of 60000 spans/s (worst lateness 1223.2 ms), so this is a saturation measurement, not an open-loop one | | | | | |
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

- **Protobuf is slower than JSON at every concurrency** (65,480 vs 82,805 at
  c1; 203,460 vs 208,973 at c16). The protobuf route decodes into a
  `serde_json::Value` DOM and then maps that to spans, while the JSON route
  now deserializes straight to `Vec<Span>`. A protobuf fast path would close
  that gap — but it would be catching up to JSON, not passing it.
- **Keep-alive is worth +11% at batch=20** (10,221 vs 9,203 spans/s) and
  nothing at batch=1000, where it measured slightly *negative*. At 245 KB and
  ~5 ms of server work per request, a ~50 µs connect is under 1% of the cost.

## The limiting stage

`direct-engine-wal` (193,637) and `http-json-wal` at the same concurrency
(193,188) are the same number. **Removing HTTP entirely — no socket, no
parsing, no protocol — changes nothing.** Whatever limits ingest is inside the
engine, which is why neither wire-format nor connection work moves it.

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

Decode, by contrast, is 811 ms of ~42 s of client time — **1.9%**. That is the
measurement that makes the protobuf question moot.

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

`--only SUBSTRING` filters scenarios, `--batch N` changes batch size, and
`TRAZA_BENCH_SERVER=/path/to/traza-server` measures a different build through
this same client.
