# Configuring Traza

Every knob Traza exposes, what it does, what it costs, and how the knobs that
interact are grouped so you do not have to know the internals to set them
coherently.

Start with [Profiles](#profiles) if you want the short version: one flag sets a
coherent group of write-path defaults, and any individual flag still overrides
it.

- [Profiles](#profiles)
- [Server flags](#server-flags)
- [Environment variables](#environment-variables)
- [Library `Config`](#library-config)
- [The throughput/latency tradeoff, measured](#the-throughputlatency-tradeoff-measured)
- [Watching whether the choice is working](#watching-whether-the-choice-is-working)
- [Reproducing these numbers](#reproducing-these-numbers)

## Profiles

`--profile throughput|balanced|latency` sets the write-path knobs that only
make sense together. `balanced` is the default and is exactly the built-in
defaults, so a server with no `--profile` behaves as it always has.

| Profile | `--flush-spans` | `--wal-commit-window-us` |
|---|---:|---:|
| `throughput` | 30,000 | 500 |
| `balanced` (default) | 10,000 | off |
| `latency` | 5,000 | off |

That is the whole of it. A profile sets two values, and the reason it is worth
a flag is not the typing it saves — it is that these two knobs have
**non-monotonic** cost curves that turn on each other, so picking them
independently by intuition reliably lands on a worse configuration than either
endpoint. Two measured examples, both of which contradict the obvious guess:

- `--flush-spans 1000` is not the low-latency setting. It cannot sustain
  60,000 spans/s at all, and every threshold below 5,000 has a *worse* p99
  than 5,000 does.
- `--flush-spans 30000` on its own is worth +23% throughput; the 500 µs commit
  window on its own is worth almost nothing at this batch size. Together they
  are worth +33%, because the window only starts paying once seals are rare
  enough for fsync to be the thing in the way.

The full curves are in
[the tradeoff section](#the-throughputlatency-tradeoff-measured).

### Precedence

**An explicit flag always beats the profile, in either argument order.** These
two are identical, and both give you a 30,000-span flush window with no commit
window:

```sh
traza-server --profile throughput --wal-commit-window-us 0
traza-server --wal-commit-window-us 0 --profile throughput
```

The parser collects the profile-owned flags as "given" or "not given" during
the scan and resolves them against the profile once, at the end, so ordering
cannot matter. An explicit `0` counts as given — it means "no window", which is
a different statement from "unset", and it overrides a profile that wanted one.

Repeating `--profile` takes the last one. An unrecognized profile name is
refused at startup rather than silently ignored.

The server logs what it actually resolved, not the profile name alone, because
a profile with one knob overridden is not the profile:

```
traza-server: profile=throughput — flush-spans=30000, wal-commit-window=500us
```

### A profile only fully applies under `wal`

The two knobs a profile sets are each inert under a different durability mode,
so a profile combined with a non-default `--durability` is partly a no-op. This
is not a special case in the profile code — it falls out of what the modes do —
but it is invisible from the flag names, so:

| `--durability` | `--flush-spans` | `--wal-commit-window-us` |
|---|---|---|
| `buffered` | active | **no effect** — there is no log to fsync |
| `wal` (default) | active | active |
| `flushed` | **no effect** — every call seals a segment regardless | active |

Profiles are tuned for and measured under `wal`, which is the default and the
only mode where both knobs do anything.

### The costs that are not latency

`--flush-spans` sets how many spans sit in the write buffer before a segment is
sealed, so raising it costs two things beyond tail latency:

- **Write-buffer memory** scales with it directly. `throughput` holds up to
  30,000 spans against `balanced`'s 10,000 and `latency`'s 5,000.
- **Restart replay time.** The write-ahead log is reclaimed when a flush seals
  its spans into a segment, so a larger flush window means more log to replay
  after a restart. Watch `wal_bytes` in `/v1/stats`: it is exactly the work a
  restart would redo.

Neither is a reason to avoid `throughput` on a machine with headroom, but both
are reasons not to raise `--flush-spans` far past it "because higher was
faster" — throughput stops improving well before the memory and replay costs
do. See the [measured curve](#the-flush-spans-curve).

### What a profile deliberately does not set

**Durability.** No profile changes what an acknowledged write guarantees. This
is structural, not a convention: `Profile` carries no durability field, so
there is no value of `--profile` that can weaken the acknowledgement contract.
`--durability buffered` is lossy by design and stays something you have to ask
for by name. A "throughput" profile that quietly made writes lossy would be the
single most damaging thing this feature could do, and the type system is a
better guarantee against it than a code review.

If you want both, say both — and the flags compose exactly as you would expect:

```sh
traza-server --profile throughput --durability buffered   # explicit, and lossy
```

**Compaction.** Compaction trades ingest throughput against *search* latency,
which is a different axis from the one profiles are named for, and turning it
off is a cliff rather than a tuning choice: segment count and open file
descriptors then grow without bound. Every profile leaves it at its default,
and read-path tuning stays on `--compaction-fanout` and
`--compaction-max-segment-bytes`.

**`--max-connections`.** It is an admission-control bound, not a performance
dial. Raising it does not make anything faster; it changes the point at which
clients get `503` instead of service. The default 1024 is far above the
concurrency that saturates any machine we have measured.

**`--payload-threshold-bytes`.** It changes *where large values are stored*,
not how fast the write path runs. That is a data-layout decision tied to your
span shape, so a profile guessing at it would be guessing at your data.

### Which one to pick

Pick **`balanced`** unless you have a measurement that says otherwise. It is
the long-standing default, unchanged by this feature, so an existing
deployment that adds `--profile balanced` gets exactly what it had.

Pick **`throughput`** when you are ingest-bound and your consumers do not care
when any individual batch is acknowledged — bulk backfill, log-style firehose
ingest, a collector with a deep local queue in front of it. You are buying
sustained rate with tail latency: p99 roughly doubles.

Pick **`latency`** when a client is blocking on the acknowledgement and its
tail matters — an agent SDK with a request timeout, a synchronous exporter, a
sidecar that will drop spans if the POST is slow. You are buying a materially
better p95/p99 with peak capacity. Do not pick it hoping for a lower median;
[the median does not move](#where-the-profiles-do-not-help).

## Server flags

| Flag | Default | What it does, and what it costs |
|---|---|---|
| `--data-dir DIR` | `./data` | Directory for all state; created if missing. One writer process per directory. |
| `--host ADDR` | `127.0.0.1` | Bind address. A non-loopback bind requires `TRAZA_TOKENS` or `--allow-unauthenticated-non-loopback`. |
| `--port PORT` | `8080` | Bind port. `0` binds an ephemeral port and announces it on stderr. |
| `--profile NAME` | `balanced` | Sets `--flush-spans` and `--wal-commit-window-us` as a group. Never changes durability. See [Profiles](#profiles). |
| `--durability MODE` | `wal` | What a `200` guarantees. `buffered` is fastest and **loses acknowledged writes on a crash**; `wal` fsyncs to the log; `flushed` seals a segment per call. Costs: see the table below. |
| `--flush-spans N` | `10000` | Threshold for sealing a segment, applied to **both** the number of unique buffered spans and the number of upserts since the last seal. The seal is a stall charged to whichever write crosses the threshold, so this sets **how often you stall and for how long**. Not monotonic in either direction — see [the tradeoff](#the-throughputlatency-tradeoff-measured). |
| `--flush-wal-bytes N` | `67108864` (64 MiB) | Seals when the write-ahead log reaches this size, whatever the record counts say. The backstop that bounds restart replay time and log disk use for any span size or key distribution. `0` removes it, leaving only the record thresholds. Ignored under `--durability buffered`, which keeps no log. |
| `--wal-commit-window-us N` | `0` (off) | Delays each fsync by up to this long so more batches join it. Costs every acknowledgement in the window up to that delay. Buys nothing on an idle store and a lot on a busy one with small batches. Never weakens the guarantee: the ack still follows the fsync. |
| `--ttl-seconds N` | off | Rolling retention window. A background pass compacts expired spans every minute; annotations and payload files age out on the same window. Costs a periodic compaction pass. |
| `--max-connections N` | `1024` | Concurrent connections served; past it clients get `503` rather than being queued. Costs one thread per live connection. |
| `--payload-threshold-bytes N` | `262144` | String attribute values longer than this are offloaded to the content-addressed payload store and replaced by a reference. `0` disables. Costs an extra file write per offloaded value; saves segment size and buffer memory. **Also bounds content search**: offloading happens at ingest, before anything indexes the span, so an offloaded value is searchable only within its 256-character preview. Since segment v4 this is no longer a memory lever, so set it on record width and disk alone — and raise it, not lower it, if `?content=` matters. |
| `--no-content-index` | off (index is built) | Stops writing the per-segment word filters that make `?content=` fast. **Content search still works** — a segment without the index is scanned rather than skipped, so the same rows come back at the cost of a scan. Saves seal-time tokenization and ~0.1% of segment size. Exposed mainly so the index's value can be measured rather than assumed; see [capacity](operations/capacity.md#content-search). |
| `--tail-ring-spans N` | `8192` | Recent admissions retained in memory for [`GET /v1/tail`](guide/http-api.md#get-v1tail). This is the live tail's entire cost — no disk, no field on the stored span — and its entire replay window: a subscriber further behind than this is told it gapped rather than being silently skipped, and backfills by ordinary search. Raise it to survive longer disconnects, at roughly a few hundred bytes per span. |
| `--compaction-fanout N` | `4` | Same-size segments merged into one. `0` or `1` disables compaction entirely. Lower merges more often (fewer segments, faster search, more ingest cost); higher merges less often. |
| `--compaction-max-segment-bytes N` | `268435456` (256 MiB) | Ceiling on a merged segment. Bounds the memory a merge needs, since it materializes its inputs; the cost is a floor on how far the segment count can fall. A merge holds no engine lock, so this does not bound a stall. |
| `--ui-dir DIR` | discovered | Built dashboard to serve at `/`. Unset ⇒ `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, `./ui/dist`, first containing `index.html`. None found ⇒ the API runs and `/` 404s with build instructions. |
| `--allow-unauthenticated-non-loopback` | off | Explicitly permit an unauthenticated non-loopback bind. |

### Durability modes

| Mode | A `200` means | Cost |
|---|---|---|
| `buffered` | accepted in memory; a crash loses anything not yet flushed | fastest, **lossy by design** |
| `wal` (default) | fsynced to the write-ahead log and recovered on restart | one group-committed fsync per batch |
| `flushed` | present in a sealed segment | a segment write per call |

**Why `--flush-spans` counts upserts too.** A span is identified by
`(trace_id, span_id)`, so re-ingesting one replaces it in place. A workload
that keeps updating the same keys — retries, or spans enriched as they
complete — therefore adds log records without ever adding a buffered record.
Counting only records made the threshold unreachable for exactly that shape of
workload: the buffer stayed at a handful of spans while the log grew without
limit, and a restart had to replay all of it. Both counts, plus the byte
ceiling, is what makes "bounded recovery work" true regardless of workload.

`wal` and `flushed` issue `fsync`, which **on macOS does not flush the drive's
own write cache**. A macOS host losing power can still lose an acknowledged
write; a `kill -9`, a panic, or an OS crash cannot. On Linux `fsync` carries
the usual guarantee. See the README's durability section for the full
discussion.

## Environment variables

| Variable | Used by | Effect |
|---|---|---|
| `TRAZA_TOKENS` | `traza-server` | Bearer auth, `rw:` and `ro:` scoped. Set-but-invalid refuses startup rather than running open. Required for a non-loopback bind unless explicitly overridden. |
| `TRAZA_UI_DIR` | `traza-server` | First place searched for the built dashboard when `--ui-dir` is unset. |
| `TRAZA_SOCKET_TIMEOUT_MS` | `traza-server` | Per-read/write socket deadline; default 30,000. Primarily for tests. |
| `TRAZA_TOKEN` | `seed` | Bearer token the seeder presents. |
| `TRAZA_BENCH_SERVER` | `ingest-bench` | Path to a different `traza-server` build, so a before/after comparison runs through one client. |
| `TRAZA_BENCH_SPANS` | `bench` | Corpus size, so other sizes can be run without overwriting the published figures. |
| `TRAZA_BENCH_COMPACTION_FANOUT`, `TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES` | `bench` | Compaction settings for the read-path benchmark. |

## Library `Config`

`traza::Config` is what `Store::open` takes. `Profile::config()` returns the
same values a `--profile` would give the server.

| Field | Type | Default | Notes |
|---|---|---|---|
| `flush_spans` | `usize` | `10_000` | As `--flush-spans`: applied to unique buffered records and to upserts since the last seal. Zero disables both record thresholds. |
| `flush_wal_bytes` | `Option<u64>` | `Some(64 MiB)` | As `--flush-wal-bytes`. `None` removes the byte bound, leaving only the record thresholds. Ignored under `Durability::Buffered`, which keeps no log. |
| `ttl_seconds` | `Option<u64>` | `None` | As `--ttl-seconds`. The engine implements it; scheduling the pass is the caller's job. |
| `payload_threshold` | `Option<usize>` | `None` | As `--payload-threshold-bytes`. **The library default differs from the server's**: the library offloads nothing unless asked, while `traza-server` defaults to 262,144 bytes. |
| `durability` | `Durability` | `Wal` | As `--durability`. |
| `compaction` | `Option<CompactionConfig>` | `Some(default)` | `None` disables compaction. |
| `wal_commit_window` | `Option<Duration>` | `None` | As `--wal-commit-window-us`. |
| `content_index` | `bool` | `true` | As `--no-content-index` inverted. `false` omits the content index from sealed segments; `SpanFilter::content` still returns the same rows, by scanning. |

`CompactionConfig`:

| Field | Type | Default | Notes |
|---|---|---|---|
| `fanout` | `usize` | `4` | Same-tier segments that trigger a merge. |
| `base_bytes` | `u64` | 8 MiB | Tier-0 size ceiling; each tier is `fanout` times larger. |
| `max_segment_bytes` | `u64` | 256 MiB | Never merge into a segment larger than this. |

`Profile` exposes `parse`, `as_str`, `flush_spans`, `wal_commit_window`, and
`config`. There is deliberately no `Profile` → `Durability` mapping.

## The throughput/latency tradeoff, measured

### How these were measured, and why it matters

The obvious way to benchmark this is a fixed pool of workers each sending as
fast as it can. That measures throughput correctly and **measures latency
almost meaninglessly**: with saturating workers, Little's law pins latency to
`concurrency / throughput`, so any configuration that raises throughput
automatically shows "lower latency" and a genuinely latency-tuned configuration
is indistinguishable from a merely fast one.

So `ingest-bench` measures both ways:

- **Closed loop** (default): workers saturate. This is the honest measure of
  **throughput**, and its latency column is reported but should be read only
  between rows at the same concurrency.
- **Open loop** (`--offered-rate N`): batches depart on a fixed schedule
  regardless of what the server is doing, and each sample is measured **from
  the time the batch was due, not from when it was actually sent**, so a
  backed-up client cannot hide behind coordinated omission. This is the honest
  measure of **latency**. A run that falls more than ten batch-intervals behind
  schedule is rejected outright rather than reported, because at that point it
  is a saturation measurement wearing an open-loop label.

Everything below is `wal` durability, JSON over HTTP with keep-alive on,
compaction disabled during the run (no profile changes it, so holding it off
keeps the comparison to the knobs the profile actually sets), a fresh data
directory per run, and payloads generated before the clock starts.

Two consequences of that setup worth stating before the numbers:

- **Compaction is off, so these absolute rates are higher than a default
  deployment sees.** Compaction costs roughly 31% of ingest throughput at its
  defaults. The profile *comparison* is the point; the absolute ceiling is not.
- Scenarios run **round-robin**, one repeat of each per round, so every
  configuration samples the same background load rather than whichever one
  happened to run during a spike.

### Load conditions, stated

These runs were **not** made on an idle machine, and the numbers should be read
knowing that. The reference box is an Apple M1 Max (10 hardware threads,
32 GB). Sampled every 15 s across the measurement window, the 1-minute load
average ran **min 6.5, mean 15.4, max 47.8**. Two contributors: the developer's
own unrelated software (a VM, dev servers, browsers), and — for the earlier and
most heavily loaded part of the window — **a second agent running 1M-span
benchmarks of this same codebase on the same host**. That second load was
scheduling, not an environmental fact about Traza, and it is the reason several
rows below carry a min far under their median.

The mitigations are structural rather than cosmetic:

- Scenarios run **round-robin**, so every configuration samples the same load
  over time rather than one of them owning a spike.
- Their order is **rotated each round**, so no configuration is pinned to the
  same phase of a periodic load.
- Every row reports **min and max** alongside the median, so the spread is
  visible instead of implied.

The consequence: **relative comparisons between rows are sound; absolute rates
are pessimistic.** Where a row's spread is wide, it is wide because rounds
executed under materially different load, and the median mixes those
conditions.

### The two knobs interact — the measured grid

1,000,000 spans per run, batch 1000, median of 5 rotated rounds.

| `--flush-spans` | `--wal-commit-window-us` | c8 median spans/s | (min–max) | c16 median spans/s | (min–max) |
|---:|---:|---:|---|---:|---|
| 10,000 | 0 | 181,980 | 143,090–193,791 | 200,517 | 147,031–208,960 |
| 20,000 | 0 | 211,431 | 165,377–222,315 | 221,398 | 169,192–227,929 |
| 30,000 | 0 | 224,261 | 208,207–229,979 | 227,537 | 186,600–239,077 |
| 50,000 | 0 | 220,909 | 178,804–236,628 | 230,601 | 178,116–239,945 |
| 20,000 | 200 | 211,403 | 173,243–228,302 | 230,412 | 181,939–251,223 |
| 30,000 | 200 | 233,370 | 179,070–248,626 | 255,778 | 198,262–263,659 |
| **30,000** | **500** | **241,804** | 192,245–251,247 | **261,782** | 213,217–269,465 |

Read the first and last rows together: `balanced` to `throughput` is
**+33% at c8 and +31% at c16**. Neither knob gets there alone — 30,000 with no
window is +23%/+13%, and the window only pays off once seals are rare enough
that fsync is the thing in the way. That is what "these knobs turn on each
other" means concretely, and it is why they are set as a group.

`--flush-spans 50,000` is past the top: at c8 it is *slower* than 30,000 and
its p99 is far worse (162.80 ms against 101.25 ms). More is not better.

### The flush-spans curve

This is the one worth studying before touching the flag, because both
intuitions about it are wrong. "Smaller flushes mean lower latency" is wrong.
"Bigger flushes mean more throughput" is wrong past a point. The knob is
**non-monotonic in both directions**, and the turning points are not where you
would guess.

600,000 spans per run, batch 1000, concurrency 8, median of 5 rotated rounds.
Closed loop measures capacity; open loop offers a fixed 60,000 spans/s — well
under capacity for most rows — and measures the tail.

| `--flush-spans` | Capacity (spans/s) | (min–max) | Open loop p50 | p95 | **p99** |
|---:|---:|---|---:|---:|---:|
| 1,000 | 36,857 | 34,270–47,715 | *cannot sustain 60k* | — | — |
| 2,000 | 57,574 | 51,035–65,681 | *cannot sustain 60k* | — | — |
| 3,000 | 74,077 | 66,940–79,731 | 33.36 ms | 48.43 ms | 56.43 ms |
| **5,000** | **97,857** | 92,938–110,838 | 27.44 ms | **47.45 ms** | **52.56 ms** |
| 10,000 | 137,859 | 133,755–147,249 | 18.40 ms | 55.99 ms | 64.00 ms |
| 20,000 | 158,409 | 104,522–210,150 | 17.93 ms | 69.73 ms | 80.33 ms |
| 30,000 | 165,329 | 149,997–225,258 | 15.66 ms | 84.04 ms | 98.22 ms |

Three things fall out of that table:

**The p99 minimum is at 5,000, and going lower makes it worse.** 3,000 has a
*higher* p99 than 5,000 despite sealing more often. A seal costs a fixed amount
regardless of size — two fsyncs, a create and rename, and a reopen-and-parse of
the result — so halving the threshold does not halve the stall, it doubles how
often you pay the fixed part. That is why `latency` is 5,000 and not the
smallest number available.

**Below 2,000 the store cannot keep up at all.** At `--flush-spans 1000` the
harness could deliver only 37,389 of an offered 60,000 spans/s and the run was
rejected rather than reported. Someone reaching for "the lowest latency
setting" would land here and lose a third of their throughput *and* their
latency.

**p50 and p99 move in opposite directions.** p50 falls monotonically as the
threshold rises (33.36 ms → 15.66 ms) while p99 rises (56.43 ms → 98.22 ms).
Tuning on the median alone would push you straight to a large threshold and a
bad tail. If you tune this yourself, tune on the percentile your clients
actually feel.

### Why the latency numbers here are open-loop

This is a measurement trap worth naming, because falling into it produces a
"latency" profile that is really just a slow one.

Under a closed-loop generator — a fixed pool of workers each sending as fast as
it can — Little's law fixes the relationship between the three quantities:
mean latency is concurrency divided by throughput. Concurrency is held
constant, so **latency becomes throughput's reciprocal and stops being an
independent measurement.** Anything that raises throughput "improves" latency
for free, and a configuration genuinely tuned for latency looks identical to
one that is merely fast.

The table above shows exactly this. Read the closed-loop columns and
`--flush-spans 30000` looks like the *lowest*-latency setting on offer, because
it is the fastest. Read the open-loop columns, where arrival rate is fixed at
60,000 spans/s and the server's speed no longer sets the offered load, and
30,000 has the **worst** tail in the table. The two readings disagree because
only one of them is measuring latency.

So `ingest-bench` measures both, and this document uses each for one purpose
only: closed loop for capacity, open loop for latency. Open-loop samples are
timed from **when a batch was due, not when it was actually sent**, so a client
that falls behind because the previous request was slow carries that lateness
into its own sample instead of hiding it — the coordinated-omission correction.
A run that fails to deliver its offered rate is rejected rather than reported,
which is what the two blank rows above are.

### Where the profiles do not help

Every one of these is measured, and several cut against the feature.

**`latency` does not lower your median — it raises it.** At a fixed 60,000
spans/s the `latency` profile's p50 is 27.44 ms against `balanced`'s 18.40 ms.
What it buys is the tail: p95 47.45 ms against 55.99 ms, p99 52.56 ms against
64.00 ms. That is the entire value proposition, and if your clients care about
the median rather than the tail, this profile is a downgrade. Pick it for a
timeout budget, never for "feels faster".

**`throughput` costs about 29% of peak capacity to undo.** The `latency`
profile's capacity is 97,857 spans/s against `balanced`'s 137,859 at the same
concurrency. If your steady-state load is anywhere near your capacity, the
`latency` profile will put you over it, and its better tail evaporates the
moment you saturate — a saturated queue's tail is set by the queue, not by the
seal size.

**The commit window is nearly worthless on its own at large batches.** At
batch 1000, `--wal-commit-window-us 500` on top of the default `--flush-spans`
is within noise. It is worth +8% only once `--flush-spans` is at 30,000
(224,261 → 241,804 at c8). The window amortizes fsync across batches that are
waiting; when seals are frequent enough to be the bottleneck, there is nothing
for it to amortize. This is why the two are set together and why setting the
window alone is close to a no-op.

**That was measured before sealing moved off the writer lock (v0.19.0), and
the window has a second effect that measurement could not see.** An `fsync` in
flight blocks concurrent `write` calls to the same file in the kernel, so
fsync frequency appears to set the cost of the log *append*, not just the cost
of the sync. Measured on macOS/APFS at concurrency 8: the same append costs
**0.076 ms at concurrency 1 and 1.778 ms at concurrency 8** — a 23x difference
in one syscall, with Traza's own log lock measured at zero wait, so the
serialization is not the engine's. Widening the window from off to 2,000 µs
cut fsyncs by 40% (112 to 67) and the mean append by **57%** (2.761 to
1.176 ms).

*Scope of that claim:* the correlation is measured and reproducible here; the
mechanism — an in-flight `fsync` blocking concurrent `write` calls to the same
file inside the kernel — is the explanation those numbers fit, not something
this measurement isolates. It has been observed only on macOS/APFS, on one
machine, and the underlying figures live in prose rather than a committed
record. Treat the lever as real and the causal story as provisional; on Linux
or another filesystem, re-measure before assuming it transfers.

So on a write-heavy deployment the window buys back append time as well as
sync time. `traza_wal_write_syscall_ns_*` and `traza_wal_lock_wait_ns_*` in
[`/v1/metrics`](operations/monitoring.md) separate the two: a large
`wal_write` with a near-zero `wal_lock_wait` is the kernel serializing against
fsync, and the lever for it is fsync frequency, not the engine.

**Batch size dominates the window's value.** The window's benefit scales with
how many batches arrive during it. At batch 1000 a 500 µs window collects
almost nothing; the effect is large only when batches are small and frequent.
If your clients send large batches — most OTLP exporters do — do not expect
this knob to do much.

**Neither profile changes the low-concurrency picture much.** At concurrency 1
there is no other work to batch an fsync with and no queue to smooth, so a
single client sees close to raw per-request cost whatever the profile says.

**Half of each profile is inert outside `--durability wal`.** See
[the interaction table](#a-profile-only-fully-applies-under-wal). Under
`buffered` the commit window does nothing; under `flushed` the flush threshold
does nothing.

### The profiles compared

One run, 1,000,000 spans, batch 1000, median of 5 rotated rounds. Same corpus,
same protocol, same durability; the only difference between rows at a given
concurrency is `--profile`.

**Capacity (closed loop, saturating):**

| Concurrency | `throughput` | `balanced` | `latency` |
|---:|---:|---:|---:|
| 1 | 85,010 | 80,791 | 74,823 |
| 4 | 173,092 | 159,716 | 134,573 |
| 8 | **231,912** | 184,571 | 162,703 |
| 16 | **250,453** | 197,056 | 157,681 |

With spread, at the concurrency each is best measured (min–max over the 5
rounds):

| Profile | c16 median | min | max |
|---|---:|---:|---:|
| `throughput` | 250,453 | 122,768 | 261,215 |
| `balanced` | 197,056 | 114,849 | 205,010 |
| `latency` | 157,681 | 153,194 | 161,347 |

`throughput` is **+27% over `balanced` at c16** and +26% at c8. `latency` costs
**20% of capacity at c16** relative to `balanced`. The wide min–max on the
first two rows is contention, not instability: their unlucky round ran while
the host was busiest. `latency`'s narrow spread is a consequence of it being
capacity-bound well below what the machine could deliver even when loaded.

**Latency (open loop, 60,000 spans/s offered — the honest measurement):**

| Concurrency | Profile | p50 | p95 | **p99** |
|---:|---|---:|---:|---:|
| 4 | `throughput` | 15.54 ms | 83.88 ms | 108.75 ms |
| 4 | `balanced` | 18.51 ms | 48.94 ms | 79.58 ms |
| 4 | **`latency`** | 18.64 ms | **40.73 ms** | **63.03 ms** |
| 8 | `throughput` | 15.31 ms | 83.40 ms | 115.18 ms |
| 8 | `balanced` | 16.76 ms | 47.81 ms | 74.94 ms |
| 8 | **`latency`** | 19.73 ms | **40.77 ms** | 75.17 ms |

That is the tradeoff, in the direction the names promise: `latency` takes p95
from 48.94 ms to 40.73 ms at c4 (**-17%**) and p99 from 79.58 ms to 63.03 ms
(**-21%**), while `throughput` is the worst of the three on both. Note also
that `throughput` has the **best p50 and the worst tail** — a clean
demonstration of why a profile chosen on median latency would be chosen wrong.

Three rows are missing from that table because their runs were rejected rather
than reported: `balanced` at c1 delivered 54,566 of 60,000 spans/s, `latency`
at c1 delivered 42,493, and `latency` at c16 delivered 55,804. A single
connection cannot offer 60,000 spans/s against these profiles' per-request
cost, and `latency` at c16 hit its lower capacity ceiling during a loaded
round. Those are real limits, not gaps in the data.

<!-- MEASUREMENTS -->

## Watching whether the choice is working

`GET /v1/metrics` reports Prometheus text. The metrics that tell you whether
your profile is doing what you picked it for:

**Is the commit window paying for itself?**

```
traza_wal_commits_total        # acknowledged batches
traza_wal_fsync_ns_count       # actual fsyncs
```

Their ratio is the group-commit amortization factor. If it is near `1.0`, every
batch is getting its own fsync and the window is buying nothing — you are
paying the delay for no return, which is what happens on an idle or
low-concurrency store. `throughput` is only worth it when this ratio is
comfortably above the `balanced` baseline; measure both rather than assuming.

**Is sealing hurting you?**

```
traza_segment_seal_ns_count    # how many seals
traza_segment_seal_ns_sum      # total time in seals
traza_segment_seal_ns_max      # the worst single stall
```

`sum / count` is the mean stall a seal imposes, and `max` is the one your p99
is made of. If `max` is a large fraction of your latency budget, `--flush-spans`
is too high for you. If `sum` is a large fraction of wall-clock time, it is too
low — you are paying the fixed per-seal cost too often, which costs both
throughput and latency.

**Is the server actually keeping up?**

```
traza_http_connections_refused_total   # must stay 0
traza_http_connections_live            # against --max-connections
traza_http_request_ns_p99              # approximate; see below
```

A non-zero refused counter means you are past `--max-connections` and clients
are being shed — any rate measured in that state is not a sustained rate.

**Where the work is going:**

```
traza_http_decode_ns_sum       # wire -> spans
traza_writer_lock_wait_ns_sum  # contention on the writer
traza_wal_encode_ns_sum
traza_wal_write_ns_sum
traza_wal_fsync_ns_sum
traza_buffer_upsert_ns_sum
traza_segment_seal_ns_sum
```

Rank these by `_sum` to see which stage owns your time before changing any
knob.

One caveat: the `_ns_p50` and `_ns_p99` gauges are **approximate by
construction** — they are power-of-two bucket upper bounds chosen for ranking
stages, not for quantile math. They are deliberately not emitted as Prometheus
histograms, because `le` buckets would invite precision the resolution does not
support. For exact percentiles, measure from the client, which is what
`ingest-bench` does.

## Reproducing these numbers

```sh
cargo build --release

# Throughput: closed loop, saturating.
cargo run --release --bin ingest-bench -- \
  --spans 1000000 --runs 5 --concurrency 8 --only profile-

# Latency: open loop at a fixed arrival rate below capacity.
cargo run --release --bin ingest-bench -- \
  --spans 600000 --runs 5 --concurrency 8 --offered-rate 60000 --only openloop
```

`--server-arg "--flag value"` applies any server flag to every scenario, which
is how the profile constants above were chosen rather than guessed:

```sh
cargo run --release --bin ingest-bench -- \
  --only http-json-wal-c8 --server-arg "--flush-spans 20000"
```

Run it on your own hardware and your own span shape. The numbers here are one
machine, one payload shape, and one batch size; the *shapes* of the curves are
the transferable part, not the constants.
