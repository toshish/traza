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

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max |
|---|---|---|---:|---:|---:|---:|
| direct-engine-wal | — | n/a | 8 | 193,637 | 190,818 | 197,871 |
| direct-engine-buffered | — | n/a | 8 | 313,201 | 262,499 | 346,574 |
| http-json-wal | json | off | 8 | 192,999 | 190,154 | 195,988 |
| http-json-wal | json | on | 8 | 182,663 | 180,839 | 185,928 |
| http-json-wal | json | on | 1 | 82,805 | 78,076 | 86,045 |
| http-protobuf-wal | protobuf | on | 1 | 65,480 | 63,497 | 67,469 |
| http-json-wal | json | on | 4 | 155,685 | 151,525 | 156,933 |
| http-protobuf-wal | protobuf | on | 4 | 138,527 | 134,675 | 142,289 |
| http-json-wal | json | on | 8 | 193,188 | 187,734 | 196,111 |
| http-protobuf-wal | protobuf | on | 8 | 172,496 | 170,937 | 179,323 |
| http-json-wal | json | on | 16 | **208,973** | 198,848 | 218,569 |
| http-protobuf-wal | protobuf | on | 16 | 203,460 | 196,847 | 213,122 |

Client-side JSON encoding of the corpus runs at ~800k spans/s, so the client
is not the constraint anywhere in this table.

## Against the roadmap target

**Target (§1.5): 250,000 spans/s sustained in `wal` mode. Best measured:
208,973. NOT MET — 16% short.**

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

That sets a hard ceiling for this design: 1,000,000 spans / 4.706 s ≈
**212,000 spans/s**, *below the 250k target*. No amount of client concurrency
passes it, and the c16 measurement of 208,973 is essentially that ceiling.

**Of the time spent holding the lock, segment sealing is 74%** — and sealing
needs no lock at all. `write_segment` converts spans to records, encodes the
segment, writes it, fsyncs, renames, fsyncs the directory, then reopens and
parses the result, all on a private vector no other thread can reach. Only the
final push into the segment list needs exclusion.

Decode, by contrast, is 811 ms of ~42 s of client time — **1.9%**. That is the
measurement that makes the protobuf question moot.

## What the remaining 16% requires

Sealing has to move off the writer lock. Two constraints make that more than a
local change — both found by attempting it:

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
