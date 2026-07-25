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
| `latency` | 3,000 | off |

That is the whole of it. A profile sets two values, and the reason it is worth
a flag is not the typing it saves — it is that these two knobs have
**non-monotonic** cost curves that turn on each other, so picking them
independently by intuition reliably lands on a worse configuration than either
endpoint. The measurements are in
[the tradeoff section](#the-throughputlatency-tradeoff-measured); the short
version is that `--flush-spans 1000` is slower *and* higher-latency than
`--flush-spans 3000`, which is not something the flag's description would ever
tell you.

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
  30,000 spans against `balanced`'s 10,000 and `latency`'s 3,000.
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
| `--flush-spans N` | `10000` | Buffered spans that trigger sealing a segment. The seal is a stall charged to whichever write crosses the threshold, so this sets **how often you stall and for how long**. Not monotonic in either direction — see [the tradeoff](#the-throughputlatency-tradeoff-measured). |
| `--wal-commit-window-us N` | `0` (off) | Delays each fsync by up to this long so more batches join it. Costs every acknowledgement in the window up to that delay. Buys nothing on an idle store and a lot on a busy one with small batches. Never weakens the guarantee: the ack still follows the fsync. |
| `--ttl-seconds N` | off | Rolling retention window. A background pass compacts expired spans every minute; annotations and payload files age out on the same window. Costs a periodic compaction pass. |
| `--max-connections N` | `1024` | Concurrent connections served; past it clients get `503` rather than being queued. Costs one thread per live connection. |
| `--payload-threshold-bytes N` | `262144` | String attribute values longer than this are offloaded to the content-addressed payload store and replaced by a reference. `0` disables. Costs an extra file write per offloaded value; saves segment size and buffer memory. |
| `--compaction-fanout N` | `4` | Same-size segments merged into one. `0` or `1` disables compaction entirely. Lower merges more often (fewer segments, faster search, more ingest cost); higher merges less often. |
| `--compaction-max-segment-bytes N` | `268435456` (256 MiB) | Ceiling on a merged segment. Bounds merge memory (a merge materializes its inputs) and how long the segment lock is held; the cost is a floor on how far the segment count can fall. |
| `--ui-dir DIR` | discovered | Built dashboard to serve at `/`. Unset ⇒ `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, `./ui/dist`, first containing `index.html`. None found ⇒ the API runs and `/` 404s with build instructions. |
| `--allow-unauthenticated-non-loopback` | off | Explicitly permit an unauthenticated non-loopback bind. |

### Durability modes

| Mode | A `200` means | Cost |
|---|---|---|
| `buffered` | accepted in memory; a crash loses anything not yet flushed | fastest, **lossy by design** |
| `wal` (default) | fsynced to the write-ahead log and recovered on restart | one group-committed fsync per batch |
| `flushed` | present in a sealed segment | a segment write per call |

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
| `flush_spans` | `usize` | `10_000` | As `--flush-spans`. Zero disables size-triggered flushing. |
| `ttl_seconds` | `Option<u64>` | `None` | As `--ttl-seconds`. The engine implements it; scheduling the pass is the caller's job. |
| `payload_threshold` | `Option<usize>` | `None` | As `--payload-threshold-bytes`. **The library default differs from the server's**: the library offloads nothing unless asked, while `traza-server` defaults to 262,144 bytes. |
| `durability` | `Durability` | `Wal` | As `--durability`. |
| `compaction` | `Option<CompactionConfig>` | `Some(default)` | `None` disables compaction. |
| `wal_commit_window` | `Option<Duration>` | `None` | As `--wal-commit-window-us`. |

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
