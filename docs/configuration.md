# Configuring Traza

Every knob Traza exposes, what it does, what it costs, and how the knobs that
interact are grouped so you do not have to know the internals to set them
coherently.

Start with [Profiles](#profiles) if you want the short version: one flag sets a
coherent group of write-path defaults, and any individual flag still overrides
it.

- [Profiles](#profiles)
- [Server flags](#server-flags)
- [Model pricing](#model-pricing)
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
| `--host ADDR` | `127.0.0.1` | Bind address. A non-loopback bind requires configured authentication — `TRAZA_TOKENS`, or a runtime credential already in the data directory's `credentials.jsonl` — or `--allow-unauthenticated-non-loopback`. |
| `--port PORT` | `8080` | Bind port. `0` binds an ephemeral port and announces it on stderr. |
| `--profile NAME` | `balanced` | Sets `--flush-spans` and `--wal-commit-window-us` as a group. Never changes durability. See [Profiles](#profiles). |
| `--durability MODE` | `wal` | What a `200` guarantees. `buffered` is fastest and **loses acknowledged writes on a crash**; `wal` fsyncs to the log; `flushed` seals a segment per call. Costs: see the table below. |
| `--flush-spans N` | `10000` | Threshold for sealing a segment, applied to **both** the number of unique buffered spans and the number of upserts since the last seal. The seal is a stall charged to whichever write crosses the threshold, so this sets **how often you stall and for how long**. Not monotonic in either direction — see [the tradeoff](#the-throughputlatency-tradeoff-measured). |
| `--flush-wal-bytes N` | `67108864` (64 MiB) | Seals when the write-ahead log reaches this size, whatever the record counts say. The backstop that bounds restart replay time and log disk use for any span size or key distribution. `0` removes it, leaving only the record thresholds. Ignored under `--durability buffered`, which keeps no log. |
| `--max-buffer-age-seconds N` | `300` | Seals when the oldest buffered span has waited this long — the bound the volume thresholds cannot provide. A trickle workload never reaches `--flush-spans`, so without this its acknowledged spans sit in the buffer (and the restart-replay log) indefinitely, and any buffered **upsert** of a persisted span disqualifies every segment holding that key from answering aggregations out of its rollup for the whole wait — measured in production at 36 days. Under real traffic the volume thresholds fire first and this never does; `traza_segment_seals_age_total` counts when it did. `0` disables. |
| `--no-shadow-seal` | off (corrective pass is on) | Stops the store answering **observed segment-key shadowing** with a bounded deduplicating merge. Left on, an aggregation that had to decode instead of using a rollup — because the same `(trace_id, span_id)` exists in two segments — latches a flag; the maintenance tick converts it into a merge of the shadowed tail run, bounded by `--compaction-max-segment-bytes`, cooled down for 15 minutes after a successful merge, and backed off exponentially (to hourly) when nothing mergeable is in reach. `traza_shadow_merges_total` counts the merges. The trigger is the observed mechanism, never a latency threshold, and it stops firing the moment the duplicates are merged away. Buffer-caused shadowing — a buffered upsert of a persisted key — deliberately does not latch: a merge cannot retire it, and `--max-buffer-age-seconds` is what bounds it. Inert when compaction is disabled, which owns all merging. |
| `--wal-commit-window-us N` | `0` (off) | Delays each fsync by up to this long so more batches join it. Costs every acknowledgement in the window up to that delay. Buys nothing on an idle store and a lot on a busy one with small batches. Never weakens the guarantee: the ack still follows the fsync. |
| `--ttl-seconds N` | off | Rolling retention window. A background pass compacts expired spans every minute; annotations and payload files age out on the same window. Costs a periodic compaction pass. |
| `--tenant-ttl TENANT=SECONDS` | off | Per-tenant retention override, repeatable. A tenant's cutoff is its runtime limits document's `ttl_seconds` if set, else this override, else `--ttl-seconds`, else **never** — an unlisted tenant on a server with no global TTL keeps everything. `0` disables that tenant's window; an invalid tenant name refuses startup. Scores (experiment-example annotations) are exempt from TTL regardless. See [administration § Per-tenant retention](operations/administration.md#per-tenant-retention). |
| `--max-connections N` | `1024` | Concurrent connections served; past it clients get `503` rather than being queued. Costs one thread per live connection. |
| `--query-deadline-ms N` | `30000` | Wall-clock budget one query or aggregation may spend inside the engine. Past it the request answers `400 {"error": "query deadline exceeded: ..."}` and **no rows** — never a partial aggregate, which would be a wrong answer that looks like a right one. Costs a clock read per segment, per merge step on the limited query path, per few thousand decoded **or buffered** records — a rollup sidecar rebuilt on a query path included — per 16 spans of step extraction and per alignment row when a run is read as a tape, replayed or diffed, per few thousand annotations an annotation search (`GET /v1/annotations`, an experiment's scores, a golden listing) walks, and once more before a completed answer is returned. *(Amended with the v0.24.1 external-review fixes: buffered-span sweeps and sidecar rebuilds used to run outside the budget, so an unflushed corpus or a sidecar-less segment could answer in full under an exhausted budget.)* The worst measured legitimate query in this repo is ~3 s (a whole-corpus `/v1/stats/duration` over 1M spans, `examples/needle`), so 30 s bounds a runaway scan without touching measured use. **What it does not protect**: [`GET /v1/export`](guide/http-api.md#get-v1export) and [`GET /v1/tail`](guide/http-api.md#get-v1tail) are documented unbounded and exempt (the socket timeout bounds abandonment), and erasure work is exempt — a partial receipt is worse than a slow one. An [evidence pack](guide/ledger.md#evidence-packs) shares one budget across all its steps (taking in settled erasures, the chain walk, the erasure cross-check, a new generation's manifest, the file check, activity, receipts); a walk, cross-check, manifest read or file check it cuts short keeps its progress for the retry. `traza_query_deadline_exceeded_total` counts the HTTP refusals; an MCP tool's refusal surfaces in the tool result instead, uncounted. `0` disables. |
| `--payload-threshold-bytes N` | `262144` | String attribute values longer than this are offloaded to the content-addressed payload store and replaced by a reference. `0` disables. Costs an extra file write per offloaded value; saves segment size and buffer memory. **Also bounds content search**: offloading happens at ingest, before anything indexes the span, so an offloaded value is searchable only within its 256-character preview. Since segment v4 this stopped being a lever on *index* memory, so set it on record width and disk alone — and raise it, not lower it, if `?content=` matters. It does still cap how wide a single span can be, which is what `--tail-ring-bytes` is measured against. |
| `--no-content-index` | off (index is built) | Stops writing the per-segment word filters that make `?content=` fast. **Content search still works** — a segment without the index is scanned rather than skipped, so the same rows come back at the cost of a scan. Saves seal-time tokenization and ~0.1% of segment size. Exposed mainly so the index's value can be measured rather than assumed; see [capacity](operations/capacity.md#content-search). |
| `--tail-ring-spans N` | `8192` | Recent admissions retained in memory for [`GET /v1/tail`](guide/http-api.md#get-v1tail), as a count. Bounds replay depth: a subscriber further behind than this is told it gapped rather than being silently skipped. **Not a memory bound** — use `--tail-ring-bytes` for that. |
| `--tail-ring-bytes N` | `33554432` (32 MiB) | Ceiling on the span text the tail's ring holds, and **what actually bounds its memory**. Whichever of the two bounds binds first evicts. The ring becomes the sole owner of a span once a seal drops it from the write buffer, and span size varies by orders of magnitude: on narrow spans the count binds first, while on LLM spans carrying prompts this one does. The newest span is never evicted, so residency can exceed this by at most one span. Observable at [`GET /v1/metrics.json`](guide/http-api.md#get-v1metricsjson) under `tail_ring`. |
| `--max-span-attributes N` | `1024` | Admission cap: top-level attributes one span may carry. A batch with a span over it is refused whole with `400`, naming the cap and the span. `0` disables. |
| `--max-attribute-key-bytes N` | `256` | Admission cap: bytes one attribute key may have, in the span's own attributes and in its events' and links' — keys are indexed, so an enormous key is an index cost, not just bytes. `0` disables. |
| `--max-span-events N` | `1024` | Admission cap: events one span may carry. `0` disables. |
| `--max-span-bytes N` | `4194304` (4 MiB) | Admission cap: one span's serialized size **as stored** — after values over `--payload-threshold-bytes` become payload references — so a span carrying one huge prompt is fine and a span of thousands of mid-sized inline values is not. Decided before the offload writes anything, so a refused span leaves no payload file. Costs one serialization pass per span on ingest, shared with usage accounting. `0` disables. |
| `--max-batch-spans N` | `100000` | Admission cap: spans one ingest request may carry. `0` disables. |
| `--compaction-fanout N` | `4` | Same-size segments merged into one. `0` or `1` disables compaction entirely. Lower merges more often (fewer segments, faster search, more ingest cost); higher merges less often. |
| `--compaction-max-segment-bytes N` | `268435456` (256 MiB) | Ceiling on a merged segment. Bounds the memory a merge needs, since it materializes its inputs; the cost is a floor on how far the segment count can fall. A merge holds no engine lock, so this does not bound a stall. |
| `--ui-dir DIR` | discovered | Built dashboard to serve at `/`. Unset ⇒ `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, `./ui/dist`, first containing `index.html`. None found ⇒ the API runs and `/` 404s with build instructions. |
| `--mcp` | off | Serve the [Model Context Protocol](guide/mcp.md) endpoint at `POST /v1/mcp`, so an agent can read this store. Off by default: serving it exposes every stored prompt to whatever holds the token, which should be a decision somebody made. |
| `--mcp-annotations` | off | Additionally expose `record_annotation` to MCP callers holding an `rw` token. **Implies `--mcp`** — asking for the write tool and getting no endpoint would be a silent no-op. Annotations written this way are always recorded as `agent:mcp`. |
| `--mcp-allowed-origin ORIGIN` | none | A browser origin permitted to drive `/v1/mcp`, including the scheme (`https://traza.example.com`). Repeatable, and implies `--mcp`. Loopback origins are always allowed; every other one needs naming here. This is the DNS-rebinding defence, and it is operator-supplied because nothing in the request can validate an origin — a rebinding request controls the `Host` header too. Needed when the dashboard is served behind a hostname, since its MCP screen is a browser page. |
| `--mcp-max-result-bytes N` | `32768` | Ceiling on one MCP tool result or resource read, counting the **whole serialized result** — text block, `structuredContent`, and the JSON envelope around them — in UTF-8 bytes, which is what goes on the wire. **Refused below 1,024**: beneath that there is no result that both fits and conforms to the `outputSchema` its tool advertises, and failing at startup beats answering every request with something a validating client rejects. Past it, rows are dropped and the truncation is stated with the argument that would have narrowed it. Raise for a client with a large context window; lower to make an agent narrow its own queries. |
| `--mcp-max-payload-bytes N` | `262144` | Ceiling on one `get_payload` fetch, whatever the call asks for. This is what stops a single offloaded prompt filling a context window. The **effective** cap is the smaller of this and `--mcp-max-result-bytes`, so that the byte count the tool reports is the one it returns. |
| `--tape-max-bytes N` | `8388608` (8 MiB) | Offloaded payload content one tape inlines when asked to resolve `$payload` references ([`GET /v1/tapes/{run}?resolve=true`](guide/http-api.md#get-v1tapesrun)), and one replay holds when it snapshots a run ([`POST /v1/replays`](guide/http-api.md#post-v1replays)). For a tape the cut is a prefix — the first reference that does not fit, and every one after it, stays a reference and the tape says `resolved_partial: true` — and every occurrence is charged. For a replay only what it keeps is charged: model answers, and tool arguments and results. Recorded requests are resolved one at a time to be digested for strict matching, then dropped, and cost none of it. A replay whose model answers cannot all be held is refused with `409`, naming the step and saying this limit is why; a tool step past it is listed in `unreadable_tool_steps` and refused by `tools/next`. This bounds bytes, not time: resolution runs inside the request's one [`--query-deadline-ms`](#server-flags) budget, checked before every payload read, and a payload referenced from many steps is probed for tenant reachability once per request. `0` resolves nothing. See the [replay guide](guide/replay.md). |
| `--xray-stale-steps N` | `20` | Steps after which a tool result in a [context X-Ray](guide/xray.md) is reported `stale` when no model output since has drawn on it (a heuristic; see the guide). A per-request `stale_steps=` overrides it. Read-side only: nothing on the ingest path consults it. At least 1 — `0` refuses startup. |
| `--radar-max-runs N` | `20000` | The most agent runs one [change analysis](guide/radar.md) (`GET /v1/changes`, MCP `what_changed`) analyses, keeping the newest by the start of their first step. Past it, the answer says `truncated: true` and `covered_since_ns` gives the time from which runs are analysed in full; runs whose first step started before it are not analysed at all. Only agent runs count: the read never holds a plain request trace, so those cannot crowd agent runs out, and no store is too big to ask about. This bounds memory: a run held costs typically about a kilobyte and at most about 6 KB (ids, names and outcomes clipped; its first 32 step tokens, up to 64 tool counts and 4 values per dimension), and the read's first pass notes at most twice the cap, so the default holds at most about 120 MB of runs, and under 200 MB in all with the bounded name tables. It does not bound time. Reading the window runs under [`--query-deadline-ms`](#server-flags). At least `1`. |
| `--pricing FILE` | none | Per-model rates used to derive a cost for LLM spans that did not meter one. Nothing is derived without it, which is the behaviour every store had before it existed. A malformed file refuses startup rather than being ignored — silently reporting `$0.00` is the symptom the file was supplied to fix. Changing the rates invalidates rollups folded under the old ones, which costs a rebuild of the affected sidecars. See [Model pricing](#model-pricing). |
| `--fuse-max-cost-usd F` | off | The **global** [run fuse](guide/guard.md) policy's cost limit, in USD: the policy every run gets whose tenant has no service-matching or `default` policy. Priced cost — metered, else derived from `--pricing`. `0` removes the limit. |
| `--fuse-max-tokens N` | off | The global policy's token limit. `0` removes it. |
| `--fuse-max-steps N` | off | The global policy's step limit (LLM, tool and agent spans). `0` removes it. |
| `--fuse-max-duration-seconds N` | off | The global policy's duration limit, first span start to last span end by the spans' clocks. `0` removes it. |
| `--fuse-max-repeats N` | `12` | The global policy's repeat limit: how often one step fingerprint — the same call with the same input — may occur among the run's last `--fuse-repeat-window-steps` steps before the run reaches it. At least 2; `0` removes it. |
| `--fuse-repeat-window-steps N` | `50` | The steps the global policy's repeat limit counts over: a fingerprint repeats when it occurs `--fuse-max-repeats` times among the run's latest N steps, so a long session that repeats a call now and then never reaches it while a tight loop does. 1 to 1000, and at least `--fuse-max-repeats`. See [run fuses § Repeats](guide/guard.md#repeats-count-within-a-window). |
| `--fuse-max-error-streak N` | off | The global policy's limit on consecutive failed steps. `0` removes it. |
| `--fuse-max-handoff-cycles N` | off | The global policy's limit on [handoff cycles](guide/steer.md#handoff-cycles-as-a-fuse-limit): how often control may return to a named agent straight from the agent it handed off to (`A → B → A`), for the pair that does it most, since the run's last reset. `0` removes it. |
| `--fuse-action stop\|warn` | `warn` | What the global policy does at a limit. `warn` reports; `stop` makes the run's verdict `stop` until it is reset. Every policy warns at 80% of a limit either way. The default warns, because a default that stopped someone's agent would be a decision nobody made. |
| `--fuse-max-runs N` | `100000` | Live runs the fuse observer keeps in memory, least recently active evicted first, shared by every tenant; independently, at most 1,000,000 span contributions are held across them, the steps of the runs' behaviour paths (at most 192 per run, for foresight) included. An evicted run is rebuilt from the store when next asked about, but until then ingest verdicts for it see only the spans that arrive after the eviction. A run evicted while **stopped** is parked instead — its stop kept, its per-span state dropped — and up to this many runs are parked; past that the oldest parked runs are dropped, and a policy stop dropped that way is recomputed by the next rebuild under the policy then in force (a kill switch is durable and is never lost, only its place in the stopped listing). See [run fuses § What is durable](guide/guard.md#what-is-durable-and-what-is-rebuilt). |
| `--no-fuses` | off (fuses on) | Turns the run-fuse observer off entirely: ingest does no fuse work, `fuses.jsonl` is neither read nor written (so a damaged one cannot keep the server from starting), responses carry no `fuses` field or `Traza-Fuse` header, and every `/v1/fuses/*` route answers `409`. [Live runs and nudges](guide/steer.md) are guard state too: their routes answer `409` and `steer.jsonl` is neither read nor written; run topology still answers. |
| `--fate-max-runs N` | `20000` | Completed runs one [Fate Map](guide/foresight.md) cohort is built from, the newest first; older ones are dropped and counted (`bounds.runs_dropped`). A build's fold tracks at most four times this many runs across all its cohorts — a window holding more is narrowed to its most recent part that fits (`bounds.window_narrowed`), for every agent of the build alike — and holds at most 2,097,152 step records (32 bytes each, 64 MiB) whatever this says. Values below 1 are raised to 1. |
| `--fate-max-nodes N` | `50000` | Tree nodes one Fate Map build holds across its cohorts. Nodes are created level by level across every cohort — each depth, most visited first, before any cohort's next depth — so the bound cuts the long tail of deep, rare paths and no agent's tree can starve another's; a cut is reported (`bounds.node_cap`, `nodes_capped` on a map whose cohort it cut, and `cut` on the nodes below which it cut). Values below 1 are raised to 1. |
| `--fate-cache-seconds N` | `300` | How long a Fate Map build is served before the next request rebuilds it. The predictive fuses (`min_success_odds`) and the [live view's foresight](guide/steer.md#foresight-on-live-runs) read only cached maps — up to twice this old while a background thread rebuilds — and never build on the check or listing path. `0` disables the cache: every map request builds, the predictive fuses never act, live runs carry no foresight, and the guard stops keeping live runs' behaviour paths (`FuseConfig::live_paths`). Any erasure drops every cached build. |
| `--fate-tenant-maps N` | `32` | Tenants whose default-window Fate Map — the one their predictive fuses read, and that `/v1/fate/runs` and a default `/v1/fate/map` answer from — stays cached at once. These live in a pool no other request can evict from; past this many tenants the least recently used one's is rebuilt when next asked for. Other builds (a task, an explicit window, the operator view) share 8 slots of their own. At most 2 builds run at once. Values below 1 are raised to 1. |
| `--fate-build-seconds N` | `120` | Budget of a Fate Map build the background warmer makes for the predictive fuses; never less than `--query-deadline-ms`. A build that runs out is retried, after a minute or a cache lifetime, over half its window (down to under two days), widening again a step once a build finishes in under a quarter of its budget; meanwhile the fuses' foresight says why it has no map, and `traza_fate_builds_total{result="failed"}` counts it. `0` gives the warmer the query deadline alone, and no bound at all when `--query-deadline-ms 0` turns that off too. |
| `--live-window-seconds N` | `900` | How recent a run's last activity — a span arriving, or a pre-call check (one naming its `next` step) — must be for [`GET /v1/runs/live`](guide/http-api.md#get-v1runslive) and its stream to list it. At least 1. Only changes which runs are listed: live state is kept and evicted by the guard's own rules (`--fuse-max-runs`). |
| `--allow-unauthenticated-non-loopback` | off | Explicitly permit an unauthenticated non-loopback bind. **Every client that reaches the port is then an unbound admin**, and the first to call `POST /v1/admin/credentials` mints the store's first admin credential — closing the store to everyone else, persistently (a later restart with `TRAZA_TOKENS` keeps it alongside). See [the non-loopback refusal](operations/administration.md#the-non-loopback-refusal). |
| `--version`, `-V` | — | Print `traza-server <version>` and exit. |
| `--restore DIR` | — | Install a backup from `DIR` into `--data-dir`, then serve it. The backup is verified before anything is swapped and the swap commits at one `CURRENT` rename, so a failed restore leaves the prior store. See [backup and restore](operations/backup.md). |

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

## `traza-server import` and `traza-server blame`

Two client subcommands of the same binary. Neither opens a data directory a
server is using; both speak plain HTTP to the server named by `--url` (TLS
belongs in front of the server), presenting `--token` or `TRAZA_TOKEN`. See
the [recorders guide](guide/recorders.md).

**`traza-server import claude-code|codex`** records coding-agent transcripts as
runs.

| Flag | Default | What it does |
|---|---|---|
| `--path DIR` | `~/.claude/projects` / `~/.codex/sessions` | Where the transcripts are. Searched recursively (eight levels) for `*.jsonl`, and for Codex also `*.jsonl.zst`. |
| `--url URL` | `http://127.0.0.1:8080` | The server to `POST /v1/spans` to; needs `rw`. Before reading anything the import asks [`GET /v1/whoami`](guide/http-api.md#get-v1whoami): a refused or read-only credential, or a `--tenant` other than a bound credential's own, stops it there. A bound credential stamps its tenant on every span. |
| `--token TOKEN` | `$TRAZA_TOKEN` | Bearer token for `--url`. |
| `--data-dir DIR` | — | Ingest straight into a data directory instead (refused while a server holds it), with the server's default 256 KiB payload threshold. |
| `--dry-run` | off | Convert and count; send and record nothing. |
| `--tenant TENANT` | none | Tenant to stamp on spans (for an unbound credential or `--data-dir`). Refused with a bound credential bound elsewhere. |
| `--since DURATION` | all | Only sessions active within `30m`/`12h`/`7d`/`2w`: a Claude Code file counts when its newest timestamped record is that recent (Claude Code appends untimestamped bookkeeping to transcripts it has not run for months, which moves their modification time), a Codex rollout when it was modified that recently. A selected session is read whole: a Claude Code transcript with its sub-agents' files, a Codex rollout with the rollouts of its parent threads (as far as the rollouts on disk tell). |
| `--project-filter SUBSTR` | all | Only sessions whose file path or working directory contains `SUBSTR`. |
| `--watch` | off | Keep polling for appended lines and new files. Only files written to in the last 10 minutes keep their converters in memory, at most 64 at any moment (the first pass over a whole history included; past it, the file modified longest ago gives its up). An unreachable or failing server is waited out — progress is kept and the pass retried, the wait doubling from the poll interval up to 30 s — instead of ending the watch; a refused credential still ends it. |
| `--interval-ms N` | `1000` | Watch poll interval (50–60000). |
| `--state FILE` | `~/.traza/import-state.json` | Per-destination progress, so a re-run or restart sends only what is new. A destination is the server URL plus the tenant written to (a bound credential's own, else `--tenant`), or a data directory plus `--tenant`; the token itself is never stored. Written atomically. |
| `--no-state` | off | Ignore and do not write the state file: send everything (idempotent — derived ids, last write wins). |
| `--json` | off | Print the final counts as JSON instead of a summary line. |

Exit status `0`; `3` when some spans or files were refused (named in the
output); `2`, with the reason on stderr, when the import could not run — an
unreadable state file, a refused or read-only credential, a `--tenant` the
credential is not bound to, an unreachable server (a batch import only: a
`--watch` waits for it, at start-up too).

**`traza-server blame FILE:LINE`** reads line `LINE` (from 1) of `FILE` and asks
[`GET /v1/blame`](guide/http-api.md#get-v1blame) which agent step introduced
it. Inside a git repository (the nearest directory holding `.git`) it sends the
path relative to the repository root and scopes the search to this repository:
the root, the main checkout, the directories of checkouts nested inside it
(`<repo>/.claude/worktrees/*` — where Claude Code puts its worktrees — and the
parent of any listed worktree inside the repository, so a worktree removed
since still counts), and every other linked worktree git lists for it (read
from git's own files; at most 64 in all), each as a `project`. A line some
other repository's session wrote into its own file of the same name is not an
answer. Outside a repository it sends the absolute path.

| Flag | Default | What it does |
|---|---|---|
| `--url URL` | `http://127.0.0.1:8080` | The server to ask. |
| `--token TOKEN` | `$TRAZA_TOKEN` | Bearer token (`ro` suffices). |
| `--tenant TENANT` | none | Tenant scope for an unbound credential. |
| `--limit N` | `5` | Introductions to show, earliest first, any whose result was never recorded after the applied ones (at most 50). |
| `--anywhere` | off | Search every project the store holds, not only this repository and its worktrees (sessions recorded at another path, or on another machine). Each answer then names its `project`. |
| `--json` | off | Print the route's JSON instead of the human rendering. |

Exit status `0` when an introduction was found (applied, or unconfirmed: its
result was never recorded), `1` when no recorded edit introduced the line (a
failed edit is none), `2` (reason on stderr) when it could not ask.

## Model pricing

OpenTelemetry defines no cost attribute, so a span carries one only if the
pipeline that produced it metered it — and most do not. Without rates, a store
that knows the model and both token counts still reports `$0.00` on every cost
surface, which reads as *this was free* rather than *nobody told me the rates*.

`--pricing FILE` supplies them:

```json
{
  "models": {
    "gpt-5.6-sol":   {"input_per_mtok": 1.25, "output_per_mtok": 10.00},
    "gpt-4o-mini*":  {"input_per_mtok": 0.15, "output_per_mtok": 0.60},
    "gpt-4o*":       {"input_per_mtok": 2.50, "output_per_mtok": 10.00,
                      "cache_read_per_mtok": 1.25},
    "claude-sonnet-*": {"input_per_mtok": 3.00, "output_per_mtok": 15.00,
                        "cache_read_per_mtok": 0.30, "cache_write_per_mtok": 3.75}
  }
}
```

Rates are **USD per million tokens**, the unit vendors quote, so the file can
be checked against a price page without arithmetic. A key ending in `*` is a
prefix pattern; the longest match wins regardless of the order the file lists
them in, and an exact name beats every pattern. A bare `"*"` is a default rate
for everything unnamed.

There is no built-in table, and there will not be one: prices change on the
vendor's schedule rather than Traza's, and self-hosted models have no public
rate at all.

**What derived cost is.** Arithmetic over the tokens the span reported, at the
rates you configured. It is not a bill — it knows nothing about batch tiers,
reasoning tokens billed at a third rate, or negotiated pricing.

**Cached prompt tokens** are priced only when the model's entry names their
rates: `cache_read_per_mtok` and `cache_write_per_mtok`, both optional, both
USD per million tokens. Providers count them differently, and the provider a
span names decides how they are applied (the table in
[Prompt caching](llm-semantics.md#prompt-caching-and-what-prompt-tokens-means)):

| Provider (`gen_ai.provider.name`) | The prompt count is | Derived cost |
|---|---|---|
| `anthropic`, `aws.bedrock`; `gcp.vertex_ai` for a model whose name contains `claude` | the uncached remainder | prompt × input rate + cache reads × read rate + cache writes × write rate + output |
| `openai`, `azure.ai.openai`; `gcp.vertex_ai` for a model whose name contains `gemini` | inclusive of the cached tokens | (prompt − cached) × input rate + cache reads × read rate + cache writes × write rate + output |
| anything else | unknown | prompt × input rate + output, as before |

Without the cache rates — or for a reported cache counter whose rate is
missing, a provider whose convention is not known, or cache counters larger
than the inclusive prompt count they are part of — the old arithmetic stands:
prompt and output at the two base rates. That **under-counts** an
Anthropic-style call (its cached tokens are never priced) and **over-counts** an
OpenAI-style one (its cached tokens are priced at the full input rate), which
is why the cache rates exist. A table without them digests exactly as before,
so upgrading invalidates no rollups; adding one invalidates those folded under
the old arithmetic, as any rate change does. The same rates price the
[cache autopsy](guide/xray.md#cache-autopsy)'s lost-cache estimate.

Three rules keep it from being mistaken for one:

- **A metered cost always wins.** If the span carries `llm.cost_usd`, that is
  the number and the table is not consulted.
- **Both directions must be known.** A span that reported only a total has no
  input/output split, and the two are priced differently, so it stays
  unpriced rather than being split by an assumed ratio.
- **The provenance is reported, not implied.** Every cost-bearing row —
  `/v1/stats/llm`, `/v1/sessions`, and each bucket of `/v1/stats/series` —
  carries `cost_derived_usd` beside `cost_usd`, plus three call counts:
  `cost_metered_calls`, `cost_derived_calls`, and `cost_unpriced_calls`.

**Judge a total by the counts, never by the dollars.** `cost_derived_usd == 0`
does not mean the figure was measured: a zero-rate model is priced and adds
nothing, and a call nothing could price also adds nothing. The three cases are
only distinguishable by the counts:

| What happened | `cost_usd` | `cost_derived_usd` | The counts |
|---|---|---|---|
| Metered by the span | 0.42 | 0 | `metered: 1` |
| Priced from the table | 0.42 | 0.42 | `derived: 1` |
| Priced at a zero rate | 0 | 0 | `derived: 1` |
| Nothing could price it | 0 | 0 | `unpriced: 1` |

`cost_derived_calls > 0` means the total is an **estimate**;
`cost_unpriced_calls > 0` means it is an **undercount**, because those calls
contributed nothing to it. The dashboard prefixes an estimate with `~`, shows
`—` rather than `0.0000` when nothing could be priced, and gives the full
breakdown on hover. MCP does the same, and its `analyze_cost` no longer claims
cost is exact when a rate table contributed to it.

### Changing the rates

A rollup sidecar records the fingerprint of the table its counters were folded
under, so editing the file invalidates exactly the cached counters that would
now be wrong, and they are rebuilt on next read. Without that, a sealed
segment would keep reporting last month's prices forever — nothing about a
stale rollup looks wrong. Removing `--pricing` likewise returns the store to
metered-cost-only rather than leaving derived figures behind.

## Environment variables

| Variable | Used by | Effect |
|---|---|---|
| `TRAZA_TOKENS` | `traza-server` | Bearer auth: comma-separated `scope[@tenant]:token` entries with `ro`, `rw`, and `admin` scopes. The optional `@tenant` binds the credential to one tenant — it then reads and writes only that tenant's data, on every surface. Set-but-invalid (including an invalid binding, or a token already registered as a runtime credential) refuses startup rather than running open. These coexist with runtime credentials minted over [`POST /v1/admin/credentials`](guide/http-api.md#post-v1admincredentials), listed as `source: "env"`. A non-loopback bind needs this or a runtime credential, unless explicitly overridden. |
| `TRAZA_UI_DIR` | `traza-server` | First place searched for the built dashboard when `--ui-dir` is unset. |
| `TRAZA_SOCKET_TIMEOUT_MS` | `traza-server` | Per-read/write socket deadline; default 30,000. Primarily for tests. |
| `TRAZA_TEST_PANIC` | `traza-server` | Latched once at startup; set to anything but empty or `0`, `GET /v1/test-panic` exists and panics its handler — the reachable panic the slot-guard tests need. Unset, empty, or `0` — the same "`0` disables" convention as every other knob — the route 404s as always. Primarily for tests. |
| `TRAZA_TOKEN` | `seed`, `traza-server mcp`, `traza-server import`, `traza-server blame` | Bearer token the client presents when no `--token` is given. |
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
| `max_buffer_age` | `Option<Duration>` | `Some(300 s)` | As `--max-buffer-age-seconds`. Enforced opportunistically on the ingest path and by `Store::maintain_buffer`, whose scheduling — like TTL's — is the embedder's job; `traza-server` calls it from its maintenance tick. |
| `shadow_seal` | `bool` | `true` | As `--no-shadow-seal` inverted. The corrective merge runs inside `Store::maintain_buffer`, so it too needs the caller to own a clock; inert when `compaction` is `None`. |
| `ttl_seconds` | `Option<u64>` | `None` | As `--ttl-seconds`. The engine implements it; scheduling the pass is the caller's job. |
| `tenant_ttl_seconds` | `HashMap<String, u64>` | empty | As `--tenant-ttl`, keyed by tenant name. A listed tenant expires on its own window; an unlisted one falls back to `ttl_seconds` — and if that is `None`, never expires, whatever other tenants are configured with. A runtime override set with `Store::set_tenant_limits` takes precedence over this map; `Store::effective_ttl` reports which layer decides a tenant's window. |
| `payload_threshold` | `Option<usize>` | `None` | As `--payload-threshold-bytes`. **The library default differs from the server's**: the library offloads nothing unless asked, while `traza-server` defaults to 262,144 bytes. |
| `durability` | `Durability` | `Wal` | As `--durability`. |
| `compaction` | `Option<CompactionConfig>` | `Some(default)` | `None` disables compaction. |
| `wal_commit_window` | `Option<Duration>` | `None` | As `--wal-commit-window-us`. |
| `content_index` | `bool` | `true` | As `--no-content-index` inverted. `false` omits the content index from sealed segments; `SpanFilter::content` still returns the same rows, by scanning. |
| `query_deadline` | `Option<Duration>` | `None` | As `--query-deadline-ms` — **the library default differs from the server's**: `None` is the unbounded behaviour every embedder had before this existed, and embedders opt in. An exhausted budget is `Error::DeadlineExceeded` and no rows, never a partial answer. `SnapshotView` paging (what export streams through), the tail, and erasure work stay outside it. |
| `tail_ring_spans` | `usize` | `8_192` | As `--tail-ring-spans`: the live tail's replay depth, as a count. Not a memory bound. |
| `tail_ring_bytes` | `usize` | `33_554_432` (32 MiB) | As `--tail-ring-bytes`: what actually bounds the tail's memory. Whichever of the two binds first evicts. `Store::tail_usage` reports current residency against both. |
| `pricing` | `Arc<Pricing>` | empty | As `--pricing`, already parsed. Empty derives nothing. `Pricing::parse` reads the documented JSON; see [Model pricing](#model-pricing). |
| `max_span_attributes` | `Option<usize>` | `Some(1024)` | As `--max-span-attributes`; `None` disables. **The admission caps apply to embedders too**: a violating batch is `Error::Invalid` naming the cap and the span, and nothing from it is stored. |
| `max_attribute_key_bytes` | `Option<usize>` | `Some(256)` | As `--max-attribute-key-bytes`. |
| `max_span_events` | `Option<usize>` | `Some(1024)` | As `--max-span-events`. |
| `max_span_bytes` | `Option<usize>` | `Some(4 MiB)` | As `--max-span-bytes`: measured as stored, after offload under `payload_threshold`. |
| `max_batch_spans` | `Option<usize>` | `Some(100_000)` | As `--max-batch-spans`: one `ingest_batch` call may carry at most this many spans. |
| `fuses` | `guard::FuseConfig` | on, warn at 12 repeats | The [run fuses](guide/guard.md): see `FuseConfig` below. On by default for embedders too — every acknowledged batch feeds the observer, which follows runs from their first step. |
| `fate` | `fate::FateConfig` | see below | The [Fate Map](guide/foresight.md)'s bounds and cache: see `FateConfig` below. |
| `steer` | `steer::SteerConfig` | 900 s window | [Steering](guide/steer.md): `live_window: Duration`, as `--live-window-seconds`. Live runs and nudges exist exactly when `fuses.enabled` does; `steer.jsonl` is opened only then. |
| `xray_stale_steps` | `usize` | `20` | As `--xray-stale-steps`: the default stale threshold of `Store::xray` callers that take it from `Store::xray_stale_steps` (the HTTP route and the MCP tool do). A value of `0` reads as `1`. |

`CompactionConfig`:

| Field | Type | Default | Notes |
|---|---|---|---|
| `fanout` | `usize` | `4` | Same-tier segments that trigger a merge. |
| `base_bytes` | `u64` | 8 MiB | Tier-0 size ceiling; each tier is `fanout` times larger. |
| `max_segment_bytes` | `u64` | 256 MiB | Never merge into a segment larger than this. |

`guard::FuseConfig`:

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | `bool` | `true` | As `--no-fuses` inverted. `false` skips the observer and never opens `fuses.jsonl`; the `Store::fuse_*` methods then return `Error::Conflict`. |
| `limits` | `guard::Limits` | `max_repeats: Some(12)` | The global policy's limits, as the `--fuse-max-*` flags (and `repeat_window_steps`, as `--fuse-repeat-window-steps`; `None` is 50 steps). `None` is unbounded. |
| `action` | `guard::Action` | `Warn` | As `--fuse-action`. |
| `max_runs` | `usize` | `100_000` | As `--fuse-max-runs`. |
| `live_paths` | `bool` | `true` | Whether every live run keeps its behaviour path (earliest 192 steps, 48 bytes each, inside the span budget) for foresight on the live view and the predictive fuses. `false`: only runs a policy with `min_success_odds` could govern keep one. `traza-server` sets it to `false` under `--fate-cache-seconds 0`. |

`fate::FateConfig`:

| Field | Type | Default | Notes |
|---|---|---|---|
| `max_runs` | `usize` | `20_000` | As `--fate-max-runs`. |
| `max_nodes` | `usize` | `50_000` | As `--fate-max-nodes`. |
| `cache_ttl` | `Duration` | 300 s | As `--fate-cache-seconds`. The predictive fuses read only cached maps, which `traza-server` builds on a background thread; an embedder that sets `min_success_odds` on a policy calls `Store::maintain_fate` on its own cadence, or its fuses never see a map. |
| `tenant_maps` | `usize` | `32` | As `--fate-tenant-maps`. |
| `build_budget` | `Duration` | 120 s | As `--fate-build-seconds`: the budget `Store::maintain_fate` gives each build. |

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

These runs also predate the admission measurement pass, which serializes each
span once more before admitting it; see
[capacity](operations/capacity.md#capacity-and-performance) for what is and is
not known about its cost.

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
