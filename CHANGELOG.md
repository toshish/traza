# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0] - 2026-07-25

**Segment sealing no longer holds the writer lock**, which was 74% of
everything that lock was held for and 88% of a run's wall clock. Ingest rises
37-116% depending on `--flush-spans`, and `--flush-spans` stops being a
throughput setting at all.

Alongside it, four defects from an independent review of v0.17, fixed and
tested. They are symptoms of one gap — Traza has several recovery domains and
nothing names a state they all agree on — so the class fix is designed too, and
scheduled before 1.0 rather than as part of HA:
[docs/generations-design.md](docs/generations-design.md).

### Fixed

- **TTL-expired spans came back after a restart.** Expiry removed them from the
  write buffer and left the write-ahead log records that carried them intact,
  so recovery replayed the expired span and `record_count` went back up.
  Expiry now rewrites the log to exactly the spans that survived — staged and
  renamed, because the survivors are still acknowledged and must not be lost to
  a crash mid-rewrite. The expired bytes leave the disk in that pass rather than
  being marked dead, which is also what "deleted on request" has to mean.
- **Damage in the middle of the write-ahead log silently dropped acknowledged
  batches.** Replay stopped at the first length, CRC or JSON failure and
  returned everything before it as if that were the whole log: three fsynced
  frames with a corrupt second one recovered as one, and the third vanished
  without a word. Recovery now distinguishes the two cases. A frame missing
  bytes it declared can only be the interrupted final append — it is dropped,
  and the file is truncated back to the last complete frame so those bytes
  cannot become interior bytes after the next append. A frame that is complete
  but fails its checksum or decode fails the open, naming the byte offset and
  what moving the log aside would cost. See
  [when the log will not open](docs/operations/durability.md#when-the-log-will-not-open).
- **A concurrent export was not one dataset.** `GET /v1/export` ran an
  independent query per 4,096-row page, so a span re-ingested behind the cursor
  was emitted twice — 5,001 rows and two versions of one primary key, under
  `X-Traza-Export-Complete: true`. Export now pins a `SnapshotView` and pages
  that: the write buffer is copied and the segment set is reference-counted, so
  compaction and expiry may unlink files the export is still reading and the
  space returns when it finishes. `complete: true` now means "this is the whole
  dataset as of the first byte", each primary key appearing at most once.
- **Hot-key updates grew the log without bound.** The automatic flush threshold
  counted unique buffered records, which an update to an existing key never
  advances: with `--flush-spans 2`, 500 acknowledged updates to one key left
  `buffered_records: 1`, `segment_count: 0` and a log of 108 KB that would never
  seal. `--flush-spans` now applies to upserts since the last seal as well as to
  unique records, and a new `--flush-wal-bytes` (default 64 MiB) bounds the log
  directly. Recovery also streams frames instead of reading the whole log into
  memory.
- **A failed expiry was not retryable, and resurrected spans.** Expiry mutated
  memory before the durable change it corresponded to: it dropped the span from
  the write buffer before the log rewrite succeeded, and removed a fully
  expired segment from the live list before unlinking its file. Either failure
  left memory ahead of the recovery authority — and left nothing for the retry
  to find, so the next pass reported `Ok(0)`, never repaired the log or the
  file, and the restart brought the data back. Both now change durable state
  first and memory second, with the in-memory step infallible, so a failed
  expiry leaves the store exactly as retryable as it found it. `Wal::rewrite`
  additionally moves every fallible step before its rename, so its failure is
  never ambiguous about which log is live.
- **A deleted segment file was reported gone before the deletion was durable.**
  An unlink is visible immediately but survives a crash only once the directory
  entry it removed is synced, so expiry could report a segment deleted — and
  drop it from the live list — over a file a crash would bring back, spans and
  all. Expiry and compaction now sync the directory after unlinking and before
  anything downstream depends on it, and the unlink is idempotent so a retry
  after a partial one can finish instead of failing forever on `NotFound`.
- **Retention rewrote segments under a new id**, which moved them to the newest
  position in an order that *is* recency order — so after a partial expiry a
  re-ingested span could revert to an older version held by the rewritten
  segment. The survivors are renamed onto the same name now, keeping the
  segment's place. Found while fixing the above.

### Changed

- **Segment sealing no longer holds the writer lock.** It was the largest thing
  the engine did while holding the lock every ingesting thread needs:
  converting spans to records, encoding the segment, writing it, fsyncing it,
  renaming it, fsyncing the directory and reopening the result — all on a
  private vector no other thread can reach. Measured at concurrency 8 before
  the change, the lock was held 88% of a run at the default `--flush-spans`
  and **74% of that was the seal**; at `--flush-spans 5000` it was 97% and 81%.
  A seal now drains the buffer under a short lock, does every byte of I/O with
  nothing held, and publishes under a short lock — the shape compaction and
  retention already had.

  Before and after with the two builds alternated round-robin on a contended
  host, median of four rounds at concurrency 8: `--profile throughput`
  162,763 → **222,683** spans/s (+37%), `balanced` 116,612 → **176,004**
  (+51%), `latency` 83,400 → **180,331** (+116%). Those levels are depressed
  by background load; the round-robin is what makes the ratios trustworthy.
  **`--flush-spans` has stopped being a throughput knob** — `latency` and
  `balanced` now land within 3% of each other, where they used to span 2x — so
  set it for the tail latency and buffer memory you want.

  Two consequences worth knowing:
  - **The write buffer can exceed `--flush-spans`** while a seal is in flight,
    because ingest no longer waits for one. Past four times the threshold an
    ingesting thread waits for the seal to publish, so it stays bounded — but
    size memory for that bound.
  - **`--flush-wal-bytes` now governs the log's real size** under sustained
    ingest. A seal that empties the buffer still discards the whole log, so a
    quiet store is unchanged; a busy one lets the log run to that bound between
    reclamations rather than emptying it on every seal, because rewriting the
    log to the survivors every time would put thousands of re-serialized spans
    straight back under the writer lock. Restart replay is bounded by the
    setting, which is what it always documented.

  What made this safe rather than fast-and-wrong: the drain **copies** the
  buffer instead of emptying it, so already-acknowledged spans are never in
  neither the buffer nor a segment — a merge keeps its inputs live until its
  output is published, and a seal now does the same. The buffer holds
  `Arc<Span>` so that copy is pointer-sized and so the post-publish eviction
  can ask *is the value under this key still the one I sealed* by handle
  identity. Comparing values would have destroyed data: a span re-ingested
  unchanged during a seal is a newer version that happens to look identical.
  `tests/seal_concurrency.rs` races reads, ingest and expiry against a seal;
  `tests/durability.rs` adds a SIGKILL taken mid-seal.
- **`traza_segment_seal_locked_ns_*` and `traza_segment_seals_coalesced_total`**
  are new on `/v1/metrics`. The first is the part of a seal that holds an
  engine lock; against `traza_segment_seal` it is the only way to see that the
  write is off the lock, because query results are identical either way. The
  second counts seals that found another already in flight and declined to
  start a second one.
- **Compaction and retention no longer stop the server.** Both held the segment
  lock across parsing every input, materializing the result and fsyncing it;
  queries waited on that lock while holding the writer lock, so ingest queued
  behind the queries. A merge measured in gigabytes was an outage measured in
  gigabytes. Both now pin their inputs, do every byte of I/O with no engine lock
  held, and take the lock back only to publish — after re-checking that what
  they pinned is still there. A new maintenance lock serializes the two against
  each other, and only against each other. `tests/compaction.rs` measures it:
  the slowest read or ingest during a merge must be a fraction of the merge.
  What this buys is that reads and ingest no longer *wait* on maintenance; they
  still share CPU and disk with it, and that contention remains unmeasured.
- **`Store::snapshot`** is public API, returning a `SnapshotView` that answers
  from one pinned instant however the store changes afterwards. Any multi-step
  read should use it; a lock cannot span pages.
- **`Error::WalCorrupt`** is a new variant, for the refusal above.
- **`Config::flush_wal_bytes`** is a new field (`Some(64 MiB)` by default). A
  `Config` built by struct literal needs it; `..Config::default()` does not.
  Documented in the library `Config` table alongside the server flag.
- **The generations design carries the log inside the boundary.** `CURRENT` and
  a global `wal.log` are two recovery authorities that no rename can publish
  together, so the design now stamps every frame with the generation epoch it
  belongs to, records `folded_through` in the manifest, and replays only frames
  after it. Publishing `CURRENT` is staged, renamed and **directory-fsynced
  before a single folded frame is reclaimed** — a rename is not crash-durable
  until then, and a durable log reclamation against a `CURRENT` that rolls back
  is the one combination that loses acknowledged writes. The crash matrix
  covers both sides of that fsync, and reclaiming folded frames is described as
  the roll-over it has to be: they are a prefix, and truncation only removes a
  suffix.

## [0.18.0] - 2026-07-25

Search stops answering real questions with silence, and gains the predicates
its own analytics already implied.

Search gains the predicates its analytics already implied, and stops
answering two classes of question with a silent empty result.

### Added

- **Range, negation and ordering predicates**: `min_attr.KEY` / `max_attr.KEY`
  (numeric, reading stringified numbers too), `not_attr.KEY`,
  `max_duration_ms` / `max_duration_ns`, and `sort=duration|-duration|start|-start`.
  Token and cost analytics could already aggregate what search could not find,
  so "which calls cost more than a cent" and "the ten slowest" were
  unanswerable. `not_attr.KEY` keeps spans that lack the key entirely — "not
  known to be an error" includes spans that never recorded a status.
- **Segment timestamp ranges (format v3)**, letting a time-filtered query skip
  a segment's records. (Every segment is opened and its indexes parsed at
  store startup; pruning avoids the record reads, not the open.)
  `since`/`until` were pure post-filters, so a
  "last 15 minutes" search read every segment in the store. v2 segments are
  still read; they carry no range, are never skipped, and age out through
  compaction.

  **No latency improvement is claimed for this yet.** Pruning is verified to
  skip the right segments by counter, but an attempt to measure the payoff
  produced a *negative* result (a windowed query slower than an unwindowed
  one) on a 40-segment store under load. Forty segments is far too few for
  per-segment probe cost to dominate — the compaction work needed thousands
  before the effect was visible — so the benchmark was measuring noise. The
  mechanism is sound and the work avoided is real; the latency benefit is
  unmeasured, and is recorded as unmeasured rather than assumed.
- **`traza_segments_pruned_by_time_total`** and
  **`traza_segments_examined_total`** in `/v1/metrics`. Pruning is invisible
  from results — a skipped segment and a scanned one give the same answer — so
  these are the only way to see it working.

### Changed

- **Attribute filters match scalars by value, not by type.** `attr.code=200`
  now finds spans that stored `200` and spans that stored `"200"`. Previously
  only the JSON reading matched, so a store of stringified codes answered
  every such query with an empty array indistinguishable from no-such-data.
  Containers still compare structurally.
- **The index probe is chosen by selectivity rather than by a fixed order.**
  Only one predicate can drive a scan; the planner took `service`, then
  `name`, then whichever attribute came first. `service` is usually the least
  selective term in a trace store, so adding a precise attribute filter to a
  service query made it *slower* — it read every span of the service and
  discarded almost all of them. The smallest posting list now wins.

### Fixed

- A sorted query ranks **every** match rather than the first page. Sorting
  cannot stream, so past an internal candidate ceiling the query is refused
  with `400` and guidance to narrow it — a "ten slowest" computed over an
  arbitrary first page is a wrong answer that looks like a right one.

## [0.17.0] - 2026-07-25

Ingest throughput roughly doubles at concurrency, and the record of why is
corrected in three places where it was wrong. Persistent connections, OTLP
decoded straight to spans for both wire formats, `--profile` for the
throughput/latency tradeoff, and a documentation set for users, operators and
developers.

Ingest throughput: 108,881 -> 208,973 spans/s at 16 concurrent clients in
`wal` mode, measured through one client against both builds. The roadmap's
250k target is still 16% away, and the benchmark now says exactly why.

### Added

- **Persistent HTTP connections.** Every response used to carry
  `Connection: close`, so a client paid a connect and teardown per batch.
  Keep-alive is now the default for HTTP/1.1. Worth +11% at batch=20 and
  nothing at batch=1000 — the honest number, not the hoped-for one.
- **`GET /v1/metrics`** in Prometheus text format: per-stage ingest timings
  (writer-lock wait, WAL encode/write/fsync, buffer upsert, segment seal,
  decode), request latency, and connection counters. Stage percentiles are
  power-of-two bucket bounds and are documented as approximate; they exist to
  rank stages, not to be published as latencies.
- **`--max-connections`** (default 1024), replacing `--workers`. Keep-alive
  means a connection occupies its handler until the client is done, so a fixed
  worker pool would serve N clients and leave the rest queued indefinitely.
  Past the limit clients get `503` rather than silence.
- **`--wal-commit-window-us`** (default off): holds an fsync open briefly so
  more batches join it. A latency-for-amortization trade that does not touch
  the guarantee — the fsync still precedes the acknowledgement it covers.
- **`--profile throughput|balanced|latency`**, setting the write-path knobs
  (`--flush-spans`, `--wal-commit-window-us`) as a coherent group so they can
  be chosen by intent rather than by reading the internals. An explicit flag
  always beats the profile, in either argument order. **No profile can change
  `--durability`** — a profile cannot represent one, so none can silently make
  writes lossy. Measured tradeoffs, including where each profile does *not*
  help, are in [docs/configuration.md](docs/configuration.md).
- **A documentation set for three audiences** under `docs/`: a user guide
  (getting started, data model, ingest, full HTTP API reference, trace
  browser), operations (deployment, durability, administration, monitoring,
  capacity), and internals (architecture, the load-bearing invariants, module
  map, testing, benchmarking). The README is now an overview that routes
  onward rather than holding all of it.
- **`ingest-bench`**, a benchmark matrix over protocol, keep-alive,
  concurrency and durability. Reports the median of N runs with its spread,
  refuses to report a rate from a run that shed a connection or stored fewer
  spans than it acknowledged, and restarts the server to re-verify every
  non-`buffered` result. `TRAZA_BENCH_SERVER` points it at another build so
  before/after runs share one client.

### Changed

- **`POST /v1/spans` decodes straight to `Vec<Span>`.** It used to parse to a
  `serde_json::Value`, deep-clone the array out of it, then re-walk that DOM
  once per span — three passes and three sets of allocations for one job.
- **`POST /v1/traces` decodes straight to `Vec<Span>` too, for both
  encodings.** Protobuf lowered to the OTLP/JSON `Value` shape and OTLP JSON
  parsed into the same shape, and both then re-walked that DOM. Protobuf
  additionally hex-encoded every trace and span id through a `format!` per
  BYTE. Decode is now **9.2x cheaper for protobuf** (4,384 → 479 ns/span) and
  **1.9x cheaper for OTLP JSON** (2,377 → 1,275 ns/span), medians of 5 runs of
  1M spans at concurrency 1. Decode is ~2% of ingest cost, so this is a CPU
  and correctness result, not a throughput one. The mapping rules the two
  decoders must agree on are shared rather than duplicated, and a differential
  test pins that agreement across every `AnyValue` variant.
- **`ingest-bench` measures latency, not just throughput**, with an open-loop
  fixed-arrival-rate mode and coordinated-omission correction. Under a
  closed-loop generator every saturating configuration reports latency that is
  just concurrency over throughput, so the tradeoff a latency profile exists
  to make was not visible at all. Scenarios also run round-robin with their
  order rotated per round, so background load hits every configuration alike
  instead of landing on whichever ran during a spike.
- **`ingest-bench` separates wire format from route.** It posted JSON to
  `/v1/spans` and protobuf to `/v1/traces`, so every protocol comparison also
  contained the OTLP mapping; on that basis this project claimed protobuf was
  slower than JSON. A third protocol, `otlp-json`, holds the route fixed, and
  scenario labels now name the route. Measured properly, **protobuf decodes
  2.3–2.7x faster than OTLP JSON** on payloads 2.9x smaller. The benchmark
  also reports bytes/span and decode ns/span per scenario.
- **The WAL encodes a batch before taking the writer lock.** Serializing under
  the lock made every concurrent ingest wait on one thread's JSON encoding.
  Only the file write remains inside it.
- **Sealing a segment no longer clones the write buffer**, and puts the spans
  back if the write fails.
- Request framing is stricter now that connections persist: transfer-encoded
  bodies and duplicate `Content-Length` headers are refused rather than
  resolved, because either ambiguity lets one request be split in two with the
  remainder attributed to the client's next request.

### Fixed

- **`bytesValue` attributes are no longer dropped.** An OTLP attribute of the
  bytes variant was stored as `null` on both ingest paths, with no error and no
  warning. It is now stored as lowercase hex, the same representation trace and
  span ids use — protobuf's raw bytes and OTLP/HTTP JSON's base64 land on the
  one value, so what an attribute holds does not depend on how it arrived.
- A connection refused at the limit now reliably receives its `503`. Closing a
  socket while the client's request bytes sat unread made the kernel send RST,
  and the RST beat the response — backpressure surfaced as "connection reset
  by peer".

## [0.16.0] - 2026-07-24

Search that scales with the store: size-tiered compaction bounds the segment
count, and at a 1 GiB segment cap filtered-search p99 clears the project's own
50 ms bar at 100M spans for the first time.

### Added

- **Size-tiered compaction**, on by default and configurable with
  `--compaction-fanout` (0 disables) and `--compaction-max-segment-bytes`.
  Filtered search costs one index probe per segment, so a store that only
  appends flush-sized segments gets steadily slower to search as it grows.
  Compaction merges same-size segments to bound that count.
  - Measured over 10M spans, uncompacted vs default compaction: attribute
    filter p50 14.8 -> 2.4 ms, p95 33.4 -> 4.1 ms, p99 220 -> 14.1 ms
    (6-15x), and trace lookup p99 4.65 -> 2.28 ms. It costs ingest
    throughput, which is the trade the flag exists to let you make.
  - Measured at **100M spans** (55 GB on disk), uncompacted vs default,
    both through the same harness: attribute filter p50 155.5 -> 9.8 ms,
    p95 747.3 -> 27.1 ms, p99 1664.6 -> 72.9 ms (16-28x), trace lookup p99
    7.72 -> 1.82 ms, segments ~10,100 -> ~380. It costs about 31% of ingest
    throughput (59,025 -> 40,894 spans/s) and ~1.5 GB of resident memory for
    the merge working set.
  - With the 256 MiB default cap **the filtered-search target is missed at
    that size**: p99 72.9 ms against the project's own 50 ms bar (p50 and
    p95 are inside it). Raising `--compaction-max-segment-bytes` to 1 GiB
    clears it — **p99 22.2 ms, p95 9.3 ms, p50 2.3 ms**, with trace lookup
    p99 0.99 ms, measured on the same corpus through the same harness. The
    binding constraint was the cap, not the algorithm: it floors segment
    count near corpus/cap, and the sampled count fell from ~380 to ~100-125.
  - That cap is a real trade, not a free win. Peak RSS rises 2.0 -> 6.7 GB
    (a merge materializes its inputs, so the working set tracks the cap) and
    sustained ingest falls a further 24%, 40,894 -> 31,267 spans/s. Measured
    at 100M on one machine in a single run; untested above that size.
  - Uncompacted, every segment holds an open file descriptor — ~10,100 at
    100M, which would exhaust a default 1024-fd limit. A second reason not
    to disable compaction on large stores.
  - Only the TAIL of the segment list is merged. Segment path order IS
    recency order, and a merged segment takes a fresh (newest) id, so
    merging a run from the middle would promote its spans past segments that
    legitimately supersede them.
  - Crash safety reuses the existing supersede journal with one marker per
    input, and the merged segment is renamed into place before any input is
    deleted.
  - TTL expiry keeps its one-minute cadence; compaction runs on a 5 s tick in
    the same maintenance thread, which now also starts when only compaction
    is enabled.

### Changed

- The benchmark can vary the segment size cap:
  `TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES` passes
  `--compaction-max-segment-bytes` to the server it spawns, parallel to the
  existing `TRAZA_BENCH_COMPACTION_FANOUT`. It defaults to the real
  `CompactionConfig::default()` value rather than a literal, so the two
  cannot drift, and the configuration is named in the generated
  `BENCHMARKS.md` row. Without this the cap could not be measured at all.
- `Config` gains a `compaction` field (`None` disables).
- README performance claims corrected against a measured 10,000,000-span run.
  The previous text dated from 0.3.1 and claimed search was "effectively
  scale-independent" at p50 2.9 ms; the measured filter p50 was 14.8 ms
  across ~1000 segments. The README now states that filtered search scales
  with segment count, and reports measured RSS (0.25 GB, not 0.71 GB) and
  disk (~6 GB for the benchmark's span shape, not 2.4 GB).
- Renamed the segment module `segment_v2` → `segment` (file, module,
  `segment_error` helper, acceptance test, format doc): there is only one
  segment format, so the version in the name was redundant. The suffix
  constants flipped too — the current format is now the unmarked
  `SEGMENT_SUFFIX` (`.seg`) and the unsupported legacy JSONL suffix is
  `LEGACY_SEGMENT_SUFFIX`.
- `tests/segment_format_acceptance.rs` now asserts against the real encoder's
  output instead of a fixture it invented. The old fixture described itself as
  "deliberately independent" but was only ever checked against itself, never
  fed to the reader — so it drifted into a layout Traza has never written (a
  u32 header length at offset 12, three sections instead of four, a `.trz2`
  extension) and passed continuously while asserting nothing. Feeding it to
  `Segment::from_bytes` failed with `Corrupt("invalid v2 header length")`.
  The tests still parse the header by hand at fixed offsets — that
  independence is the point — but they parse bytes from
  `segment::encode`, round-trip them through `Segment::from_bytes` and
  `Segment::open`, and pin the four-section layout, the record encoding, and
  what the reader must reject. `reopen_persistence` no longer writes bytes
  and reads them back asserting equality (a test of `fs`, not of Traza).
- `docs/segment-format.md` describes the layout that shipped. It previously
  documented only the pre-implementation proposal — a trailing footer, a
  JSONL payload, and readable v1 segments — none of which match
  `src/segment.rs`; v1 JSONL is rejected with a migration pointer. The
  proposal is retained below the real layout, marked as history.
- Reconciled the on-disk segment magic. The encoder wrote `TRAZAV2` while the
  acceptance test and format doc expected `TRAZASEG`; they never met, because
  the acceptance fixture is self-checked and was never fed to the real reader.
  The magic is now `TRAZASEG` (the version lives in the `VERSION` field, not
  the magic), matching the docs, and the acceptance test pins it to the real
  `segment::MAGIC` constant so the two cannot drift again. **On-disk format
  change:** existing `.seg` files written before this are no longer read
  (acceptable pre-release; no data migration is provided).

## [0.15.0] - 2026-07-24

Durability: an acknowledged write now means what the deployment says it means.

### Added

- **Write-ahead log with group commit.** Ingest appends a batch to `wal.log`
  and fsyncs it BEFORE acknowledging; the log is replayed into the write
  buffer on open and reclaimed once a flush seals those spans into a segment.
  The fsync runs outside the writer lock, so concurrent batches coalesce into
  one sync instead of serializing an fsync per request — measured 13.7k
  spans/s at concurrency 1 rising to 48.1k at 16, where per-batch fsync would
  stay flat.
- **Explicit durability modes**, selected with `--durability` and reported in
  every ingest response and in `/v1/stats`, so a client never has to infer
  what a `200` promised:
  - `buffered` — accepted in memory; lossy by design, and no longer the
    default.
  - `wal` (**new default**) — fsynced to the log and recovered on restart.
  - `flushed` — present in a sealed segment.
- `/v1/stats` reports `durability` and `wal_bytes` (the work a restart would
  replay).
- `tests/durability.rs` holds each mode to its own claim under **SIGKILL**:
  `wal` and `flushed` lose nothing acknowledged, `buffered` is verified to be
  lossy rather than accidentally durable, recovery preserves last-write-wins
  for a re-ingested span, the log is reclaimed after a flush without losing
  data, and 800 spans acknowledged across 8 concurrent writers all survive.

### Changed

- `Config::default()` is now `Durability::Wal`. A store that silently loses
  acknowledged writes is the wrong default even though it is the faster one.
  `Config` gains a `durability` field, so code constructing it as a struct
  literal must add one (or spread `..Config::default()`).
- The benchmark measures the default (`wal`) and labels the mode, rather than
  reporting a `buffered` number no production deployment could rely on.
- `tests/auth.rs` no longer pins socket teardown to `ECONNRESET`; the
  invariant is that a complete HTTP response arrived, and pinning the errno
  made the test flaky under parallel load.

### Notes

- `fsync` on macOS does not flush the drive's write cache (`F_FULLFSYNC`
  would, and std does not expose it), so a macOS power cut can still lose an
  acknowledged write. Process death cannot, on any platform. Documented in
  the README and `src/wal.rs` rather than left implied.

## [0.14.0] - 2026-07-24

OpenLLMetry-native tracing, a standalone dashboard served from its build
output, and a trace browser that renders what agents actually produce.

### Added

- Traza now follows the [OpenLLMetry](https://github.com/traceloop/openllmetry)
  standard (Traceloop's OpenTelemetry GenAI conventions). Sessions, token
  analytics, and the dashboard recognize the current OTel GenAI attributes —
  `gen_ai.provider.name`, `gen_ai.operation.name`, `gen_ai.usage.input_tokens` /
  `output_tokens`, `gen_ai.request/response.model`, `gen_ai.conversation.id`,
  and `traceloop.*` — so an OpenLLMetry-instrumented app populates every
  derived view over OTLP with no attribute renaming. The OTel-deprecated names
  (`gen_ai.system`, `gen_ai.usage.prompt_tokens` / `completion_tokens`) and
  Traza's native `llm.*` / `session.id` shorthand are accepted as aliases;
  native behavior is unchanged. A new `src/semconv.rs` normalization layer is
  the single source of truth for the key precedence.
- `GET /v1/stats/llm?group_by=provider` rolls tokens up by the resolved
  provider.
- `GET /v1/spans?session=<id>` filters spans to a session, unioning every
  recognized session key so a session whose spans mix conventions (some
  `session.id`, some `gen_ai.conversation.id`) is returned whole. Sessions are
  grouped by any recognized key, and each reports the `session_attribute` that
  grouped it.
- The dashboard's span detail renders provider/model/token chips and a Messages
  panel from the current JSON `gen_ai.input.messages` / `gen_ai.output.messages`
  as well as the legacy indexed `gen_ai.prompt.*` / `gen_ai.completion.*`
  attributes and native `llm.prompt` / `llm.completion` events.

### Changed

- The dashboard is no longer compiled into `traza-server`. The server now
  serves the UI's build output from disk: `--ui-dir` (default `./ui/dist`,
  produced by `cd ui && npm run build`) backs `GET /` and `GET /dashboard`.
  Building the server still needs no Node toolchain, a rebuilt UI is picked up
  without restarting, and a missing build is not fatal — the API runs and the
  UI routes 404 with build instructions. The shell stays served before the auth
  gate (it is static build output carrying no data) while every `/v1` call it
  makes remains gated. Path traversal out of the UI directory is refused.


- License is now Apache-2.0 only (previously dual MIT OR Apache-2.0).
  `LICENSE-MIT` is removed and `LICENSE-APACHE` renamed to `LICENSE`.

- `ci.sh` is the merge bar for the whole tree: it now builds and tests the
  dashboard (`npm ci`, `npm test`, `npm run build`, Node per `ui/.nvmrc`) and
  rejects source files containing a NUL byte. Rust tooling cannot police the
  UI, and a broken Vite build must not merge green. `TRAZA_SKIP_UI=1` runs the
  Rust half alone.
- The dashboard has unit tests (`ui/`, vitest): message parsing, content-type
  detection, the markdown subset, and the syntax tokenizer — including that
  highlighting round-trips to the exact source and never linkifies a
  `javascript:` URL.

### Fixed

- Session resolution now resolves every recognized key under ONE snapshot of
  the write buffer and segment list. Querying each key separately let a span
  re-ingested between the queries be seen first in its superseded version,
  which then locked the newer version out — breaking last-write-wins during
  ordinary concurrent ingest.
- A numeric session id (`"gen_ai.conversation.id": 4711`) can now be opened,
  not just listed. Normalization stringifies numeric attributes, but the
  lookup matched only JSON strings, so such a session appeared in
  `/v1/sessions` while `/v1/sessions/4711` returned 404 and
  `/v1/spans?session=4711` returned nothing.
- The server finds a packaged dashboard: with no `--ui-dir` it searches
  `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, then
  `./ui/dist`, and lists every path it tried when none has a build. A
  CWD-relative default alone meant an installed binary served nothing unless
  it was launched from a checkout.
- The conversation view pages through long sessions and says when it is
  showing a prefix. Spans come back oldest-first, so a fixed cap silently
  dropped the newest turns while presenting the result as complete.
- `ui/src/views/ConversationView.jsx` no longer contains a literal NUL byte,
  which made git treat the file as binary and hid it from diff and blame.

### Removed

- The checked-in generated `src/dashboard.html` and the `ui/scripts/embed.mjs`
  script that produced it, along with `src/dashboard.rs`. UI builds no longer
  regenerate an embedded HTML file, so `ui/` changes no longer produce a
  368 KB diff in the Rust crate.

### Notes

- Cost analytics remain a Traza extension (`llm.cost_usd`), not part of
  OpenLLMetry — OpenTelemetry GenAI defines no cost attribute. Cost populates
  only when the ingest pipeline supplies it.

## [0.13.0] - 2026-07-23

Wire-contract release: `/v1/stats` renames its counters to record
terminology and `/v1/export` switches to chunked framing with
completion trailers — clients parsing either surface must update.

### Fixed

- Export pagination now uses the engine's exclusive full-key
  `(start_time, end_time, trace_id, span_id)` cursor with a fixed 4,096-row
  page. Equal-timestamp runs no longer trigger exponential prefix re-fetches
  or corpus-sized pages, and bounded queries borrow resident posting lists
  instead of cloning them per page.
- Export responses use HTTP chunked framing with explicit
  `X-Traza-Export-Complete` and `X-Traza-Export-Count` trailers. A storage or
  serialization failure after `200 OK` can no longer masquerade as a complete
  dataset.
- Annotation replay now tolerates only an unterminated final append. A
  malformed newline-terminated middle record fails startup instead of
  silently hiding every valid annotation after it; a torn tail is truncated
  before new appends, a missing final delimiter is restored, and annotation
  creation/rewrite renames also fsync the parent directory.
- LLM/session integer counters saturate instead of panicking in debug builds
  or wrapping in release builds. Non-finite cost strings are ignored and
  floating sums remain finite.
- Payload sweeping holds the touch-registry lock only across each final
  eligibility check and deletion. The ingest race remains excluded without a
  large directory walk stalling every oversized-payload write.

### Changed

- `/v1/stats` and `Store::stats` now name their cheap physical storage counts
  as `record_count` / `*_records`. Immutable historical versions remain
  physical records until compaction even though last-write-wins queries expose
  one logical span.
- The server now binds `127.0.0.1` by default. Unauthenticated non-loopback
  binds are refused unless the operator configures `TRAZA_TOKENS` or passes
  `--allow-unauthenticated-non-loopback` explicitly.
- Current documentation now matches the v2-only file-backed engine,
  OTLP/HTTP protobuf support, v0.12 crate line, export integrity contract, and
  safe bind defaults.

## [0.12.2] - 2026-07-23

### Fixed

- **Payload TTL race** (found in review): compaction snapshotted live
  references, released the locks, then swept — an ingest committing a
  new reference to an old deduped file inside that window authorized
  its deletion. The store is single-process (DirectoryLock), so an
  in-memory touch registry now records every payload write/dedup
  BEFORE filesystem work; the sweep spares anything touched within a
  10-minute immunity window in addition to the live-reference set.
- **Concurrent identical-payload ingest** (found in review, reproduced
  as 9 successes + one ENOENT): all writers shared one `<hash>.tmp`
  path, truncating each other's temp and racing the rename. Temps are
  now writer-unique; every rename is valid, and identical content
  makes the last rename byte-identical anyway.
- **Export truly streams** (found in review): `GET /v1/export`
  materialized the complete query result plus a complete NDJSON
  buffer, defeating the larger-than-RAM design. It now streams
  close-delimited (no Content-Length) in bounded pages keyed by the
  query's total sort order, holding no engine lock across socket
  writes; equal-timestamp runs wider than a page grow the page until
  the cursor can cross them.

## [0.12.1] - 2026-07-23

### Fixed

- **Payload TTL deleted live data** (found in review, reproduced): the
  content-addressed store dedupes identical payloads to one file
  WITHOUT refreshing its mtime, while the TTL sweep deleted by mtime
  alone — a fresh span re-referencing old content kept its span but
  lost its payload. The sweep now protects every payload referenced by
  a live span (buffer + all segments, collected via the cached
  rollups) and deletes only unreferenced-and-old files.
- **LLM/session rollups double-counted replaced spans** (found in
  review, reproduced): cached per-segment rollups summed every
  physical copy of a re-ingested (trace_id, span_id), contradicting
  the primary key's last-write-wins semantics — the aggregate said
  2 calls / 30 tokens / $0.30 where the visible truth was
  1 call / 20 tokens / $0.20. Rollups now walk segments newest-first
  carrying the seen-key set (FNV-1a prefilter; buffer always wins);
  a segment containing any possibly-superseded key is re-scanned
  exactly, dropping stale versions. Collisions can only cost an
  unnecessary re-scan, never a wrong count.

## [0.12.0] - 2026-07-23

### Added

- **Payload offloading**: string attribute values above a threshold
  (server default 256 KiB, `--payload-threshold-bytes`, `0` disables)
  are extracted at ingest to a content-addressed store
  (`payloads/<aa>/<sha256>.bin`, temp+rename writes) and replaced by
  `{"$payload": "sha256/…", "bytes": N, "preview": "…"}`. Identical
  payloads are stored once; `GET /v1/payloads/{ref}` serves the bytes
  (hex-validated — traversal-shaped refs are 404). SHA-256 is
  implemented in-crate (FIPS 180-4) and verified against the NIST
  vectors.
- **Annotations**: post-hoc scores/feedback/eval verdicts attach to
  spans (or whole traces) without mutating them — an append-only,
  fsync'd `annotations.jsonl` with an in-memory index, tolerant of a
  torn tail. `POST /v1/annotations`, `GET /v1/annotations`, and the
  trace view carries a trace's annotations alongside its spans.
- **Dataset export**: `GET /v1/export` streams any span filter as
  NDJSON (unbounded by default, unlike interactive search) — the
  traces-to-eval-dataset path.
- TTL compaction now also drops annotations older than the window and
  sweeps payload files by mtime (an orphan payload outlives its span
  by at most one TTL).

## [0.11.0] - 2026-07-23

### Added

- **OTLP/HTTP binary protobuf**: `POST /v1/traces` now accepts
  `Content-Type: application/x-protobuf` — the encoding OTel SDKs use
  with `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` — via a
  dependency-free, bounds-checked wire decoder that lowers protobuf
  into the OTLP/JSON shape, so both encodings share one mapping.
  Protobuf clients receive a protobuf-typed empty
  ExportTraceServiceResponse. Malformed payloads (truncated varints,
  lying lengths, 32-deep hostile nesting) are 400s, never panics;
  unknown fields skip per the protobuf contract. gRPC is not served.
- **Span links**: spans carry a first-class `links` array
  (`trace_id`, `span_id`, `attributes`) on the native JSON surface and
  the OTLP mapping (previously dropped) — the non-tree structure of
  agentic traces: fan-out/fan-in, retries, cross-agent causality.
- Conformance suite with an independent test-side protobuf encoder
  (`tests/otlp_protobuf.rs`).

## [0.10.0] - 2026-07-23

### Added

- **Sessions**: any span carrying a `session.id` attribute joins a
  session — the unit of agentic/LLM work spanning many traces.
  `GET /v1/sessions` lists sessions (span/trace counts, token sums,
  cost, errors, activity window), `GET /v1/sessions/{id}` adds the
  per-trace breakdown. The dashboard grew a Sessions panel; clicking a
  session filters the span table to it.
- **LLM aggregation**: `GET /v1/stats/llm?group_by=model|service|session|day`
  returns exact token/cost/error/latency rollups over an optional
  `since`/`until` window. Numeric attributes are accepted as numbers or
  numeric strings; explicit `llm.total_tokens` wins over the
  prompt+completion sum.
- Aggregation cost model: sealed segments are immutable, so per-segment
  rollups are computed once and cached; query windows that split a
  segment decode just that segment for exact edge membership. Rollups
  for superseded (compacted) segments drop out automatically.
- `session.id` added to the LLM semantic conventions
  (docs/llm-semantics.md) with a sessions/aggregation section.

## [0.9.2] - 2026-07-23

### Security / Hardening

- The engine itself now rejects spans with an empty `trace_id` or
  `span_id` (`Error::InvalidSpan`) — the primary-key invariant no
  longer depends on each HTTP surface validating correctly, and a
  batch with any invalid span stores nothing.
- Socket read/write deadline (30s, `TRAZA_SOCKET_TIMEOUT_MS` to
  override): a peer that connects and goes silent — or declares a
  body it never sends — is released instead of parking a worker
  thread forever.
- Request headers are capped at 64 KiB (bodies keep the 64 MiB cap).
  Previously a request could spend the full body budget on headers,
  doubling the per-request memory ceiling.
- With auth enabled, requests are refused from the head alone:
  an unauthenticated client can no longer make the server buffer a
  declared 64 MiB body before hearing 401.

### Added

- `tests/ingest_hardening.rs`: adversarial wire probes — lying
  content-length, oversized headers, silent peers, pre-auth body
  refusal, malformed OTLP shapes, query-parameter extremes, inverted
  timestamps, hostile ids (NUL-prefixed / unicode) round-tripping
  through flush and reopen, and the documented last-write-wins
  semantics for duplicate keys within one batch.
- Test harnesses kill their spawned server on every exit path
  (`Drop`), so a failing test reports instead of hanging `cargo test`
  on the leaked child's pipes.

## [0.9.1] - 2026-07-23

### Fixed

- `POST /v1/spans` now rejects spans with an empty `span_id` (400,
  naming the span index), matching the existing empty-`trace_id`
  rejection. Both halves of the `(trace_id, span_id)` primary key must
  be non-empty: previously two distinct spans with empty `span_id`
  were both counted in `{"accepted": N}` while the upsert silently
  collapsed them into one stored span. The OTLP endpoint already
  rejected empty ids; the native endpoint now agrees. Rejection is
  atomic — nothing from a rejected batch is stored.

### Added

- `docs/ha-design.md`: the high-availability design document — four
  compared architectures with a quorum-replicated logical-log
  recommendation (segment shipping retained for catch-up), grounded in
  the real engine mechanisms (`WriteBuffer` acknowledgment boundary,
  `segment_v2` snapshot transfer, replicated supersede-journal
  transitions, `(trace_id, span_id)` idempotency, `DirectoryLock`
  scope). Design only; no HA behavior is implemented.

## [0.9.0] - 2026-07-23

### Added

- Bundled dashboard: a dependency-free trace browser embedded in the
  server binary (`src/dashboard.html` via `include_str!`), served at
  `GET /` and `GET /dashboard`. Recent-spans view with a filter bar
  mapped 1:1 onto the `/v1/spans` query params, a trace waterfall backed
  by `/v1/traces/{id}` with error spans highlighted, and a span detail
  pane (attributes, events, parent, extra fields). Light and dark color
  schemes follow the browser preference.
- The dashboard consumes only the existing JSON API — no new endpoints.
  With `TRAZA_TOKENS` set the shell stays open (it carries no data)
  while every API call remains gated; the page prompts for a bearer
  token on the first `401` and stores it in `sessionStorage` only.
- `traza::dashboard`: the embedded asset and route helper
  (`route(path) -> Option<DashboardResponse>`), consulted by the server
  before the auth gate for `GET` requests.
- `tests/dashboard.rs`: process-level acceptance — the real server
  serves the embedded page at `/`, `/dashboard`, and `/dashboard/`
  (unknown deeper assets 404); a grep oracle proves the page references
  no external URLs (self-contained, no supply-chain surface); with auth
  enabled the shell loads open while `/v1/*` still returns 401/403/200
  by scope.

### Changed

- README: Features and Roadmap now reflect shipped OTLP ingest, auth,
  LLM-observability semantics, and the dashboard; remaining roadmap is
  streaming results, filter throughput at scale, and high availability.

## [0.8.0] - 2026-07-23

### Added
- **Roadmap leg 4 — bearer-token auth.** `TRAZA_TOKENS` (comma-separated
  `scope:token`, scopes `ro`|`rw`) requires `Authorization: Bearer` on
  every request: unknown tokens 401 with a `WWW-Authenticate: Bearer`
  challenge, insufficient scope 403, token comparison constant-time (all
  credentials checked even after a match). Unset means open — the
  development default that keeps every existing test unchanged — while a
  set-but-invalid value refuses startup rather than silently running
  open. Zero new dependencies; process-level matrix tests cover open
  mode, 401/403/200 across ingest, OTLP, and flush endpoints, and the
  startup refusal.

## [0.7.0] - 2026-07-23

### Added
- **Roadmap leg 3 — LLM-observability semantics.** Documented gen-AI span
  conventions ([docs/llm-semantics.md](docs/llm-semantics.md)): `llm.*`
  span names, model/token/temperature/stop-reason/tool/cost attributes
  (index-served like any attribute), prompt and completion payloads as
  span events so large text never enters the filter index, and four
  concrete query recipes over the existing API. Process-level tests prove
  every recipe through both `/v1/spans` and OTLP ingest. Purely additive:
  one doc, one test target, one README section.

## [0.6.0] - 2026-07-23

### Added
- **Roadmap leg 2 — OTLP/HTTP JSON ingest.** `POST /v1/traces` accepts an
  OpenTelemetry ExportTraceServiceRequest in OTLP/HTTP JSON and maps it
  onto the span model: hex ids lowercased, `*TimeUnixNano` accepted as
  string or number, typed `AnyValue` attributes (string/int/double/bool/
  array/kvlist) flattened to plain JSON, resource `service.name` becoming
  the span's service (`unknown_service` fallback), scope attributes
  merging beneath span attributes, events mapped, and OTLP status codes
  becoming `ok`/`error`/empty. Structurally invalid requests 400 with a
  diagnostic; the existing `/v1/spans` contract is untouched. No new
  dependencies. Conformance-tested end to end against the real binary,
  including index-served queries over OTLP-ingested spans.

## [0.5.0] - 2026-07-23

### Changed
- **Roadmap leg 1 — larger-than-RAM reads.** Segments are file-backed:
  `Segment::open` reads only the header and index sections into memory and
  serves every record access by reading exactly the needed byte range from
  the file (std `Seek`+`Read`; no mmap, no new dependencies). Flushing
  reopens the new segment file-backed, so no resident payload copy survives
  the write either. `Store::resident_payload_bytes()` exposes the invariant
  (zero after open and after flush). Measured at a 10M-span corpus
  (2.4 GB on disk): **0.71 GB peak server RSS** (was ~2.4 GB
  bytes-resident, ~5 GB in the pre-v2 engine); trace lookup p50 0.8 ms,
  attribute filter p50 8.7 ms — RAM is O(indexes) and stores larger than
  memory serve correctly.
- The lazy limited-query merge caches per-source head timestamps: with
  file-backed segments every peek is a disk read, and re-peeking all
  sources per pop had regressed the 10M filter to 125 ms; cached heads
  restore 9.6 ms.

### Known deviation
- The leg's relative bound ("1M latencies within 2x of 0.4.0") is missed
  on filter p95: 3.34 ms vs 1.27 ms (2.6x) — the cost of on-demand file
  reads at sub-5 ms magnitudes. The absolute gate (< 300 ms) passes with
  ~90x headroom; recorded here rather than tuned away.

## [0.4.0] - 2026-07-23

### Changed (breaking)
- **Span identity is a primary key.** (trace_id, span_id) is enforced
  unique: re-ingesting an existing pair replaces the stored span — in the
  write buffer, across flushes, and across restart. Last write wins on
  every read path (trace, filtered, and limited lazy queries), so client
  retries are idempotent and never produce duplicate copies. This reverses
  0.3.1's at-least-once visible-duplicate semantics.
- **v1 JSONL segments are no longer read.** The engine is v2-only; opening
  a directory containing a legacy `.jsonl` segment fails loudly with a
  migration pointer (read with 0.3.x first). The dual-format code path is
  removed.

## [0.3.1] - 2026-07-23

### Fixed
- **Data loss across restart**: next-segment numbering only recognized
  `.jsonl` names, so a reopened v2-only store restarted at id zero and the
  next flush renamed over an existing segment, destroying persisted spans.
  Both suffixes count now, and `write_segment` refuses to replace an
  existing file outright.
- **Acknowledged duplicate cardinality survives restart**: content-based
  duplicate healing is gone. Compaction rewrites are journaled with a
  supersede marker written before the rewrite begins; recovery finishes an
  interrupted rewrite from the journal in either direction and never
  deduplicates by content, so legitimately re-ingested identical spans keep
  both acknowledged copies.
- A corrupt v2 header with an out-of-range attribute-index offset returns
  `Error::Corrupt` instead of panicking through unsigned subtraction.
- User attributes named with a NUL prefix (for example `"\u{0}service"`)
  can no longer overwrite the reserved service/name index keys and poison
  those queries; such attributes are stored verbatim but excluded from the
  index, and filters on them decline index use symmetrically.

### Changed
- **Limited queries are lazy end to end**: per-segment index postings stay
  undecoded and a k-way merge pops candidates in start-time order, decoding
  and re-verifying only what the limit returns. Measured: attribute filter
  p50 18 ms -> 0.53 ms at 1M spans and 209 ms -> 2.9 ms at 10M — the 10M
  advisory target (<100 ms) is closed with 35x headroom.
- README limitations and roadmap reflect the v2 engine (byte residency,
  journaled compaction, remaining mmap/streaming work).

## [0.3.0] - 2026-07-23

### Changed
- **Segment format v2**: new segments are indexed binary files (`.seg`) —
  JSON span payloads with an embedded record-offset index, trace index, and
  attribute index, written with the same temp + fsync + atomic-rename
  discipline. v1 JSONL segments remain fully readable beside v2 and heal
  through the same duplicate-recovery path; TTL rewrites produce v2.
- **Byte-resident reads**: opening a store no longer materializes spans.
  v2 segments hold raw bytes plus their indexes; spans parse on demand,
  only for records a query returns. `Store::resident_persisted_span_structs`
  exposes the invariant (zero after a v2-only open).
- **Index-served queries**: `get_trace` binary-searches the trace index;
  filters narrow through service/name/attribute indexes or time range and
  re-verify every predicate on the parsed span. Measured on the bundled
  benchmark: trace lookup p50 0.185 ms at 1M spans (was 14.2 ms) and
  0.536 ms at 10M (was 145.6 ms); attribute filter p50 18 ms at 1M
  (was 66.5 ms) and 209 ms at 10M (was 4,395 ms).

### Known limits
- The 10M advisory filter target (<100 ms) is not yet met: candidate
  payload parsing dominates large result groups. Posting-list
  intersection and parse-avoidance are the next optimization.
- Segment bytes are read into memory at open (no mmap yet); resident cost
  is file bytes + indexes rather than parsed structs.

## [0.2.3] - 2026-07-22

### Fixed
- Segment writes are buffered. `serde_json::to_writer` against a raw `File`
  issued one write() syscall per JSON token, making flush cost ~140 us per
  span and capping measured end-to-end ingest at 5,450 spans/s. A 256 KiB
  `BufWriter` restores flush to ~1.5 us per span; the regenerated benchmark
  measures 138,180 spans/s and the ingest gate passes again.

## [0.2.2] - 2026-07-22

### Fixed
- `ttl_seconds: Some(0)` disables expiration as documented instead of
  expiring every existing span; the library `Config` TTL default is `None`
  as documented, no longer a silent seven days.
- Recovery heals crash-duplicated segments: exact-duplicate spans are
  dropped at open and fully-duplicate segment files deleted, closing the
  window where a crash mid-compaction returned two copies of surviving
  spans on reopen.
- An empty reclamation sentinel left by a reclaimer that died before
  recording its PID no longer wedges lock recovery: unreadable sentinels
  older than ten seconds are treated as corpses.
- The README introduction and features no longer claim a manifest,
  per-segment indexes, or log replay; the configuration tables match the
  code (server `--ttl-seconds` drives engine compaction; `--host` and
  `--flush-spans` documented).

### Changed
- BENCHMARKS.md regenerated against the engine-backed server. The ingest
  gate is honestly reported as MISSED (5,450 spans/s against the 50,000
  target); read gates pass. Closing the write-path gap is the top of the
  roadmap.

## [0.2.1] - 2026-07-22

### Fixed
- The documented ingest contract works again: timestamp aliases
  (`start_time_unix_nano`, `start_timestamp_ns`, `start_ns`, `start_time`
  and the matching `end_*` keys) are accepted, `parent_span_id`, `status`,
  `attributes`, and `events` are optional, and unknown span fields are
  stored and returned verbatim instead of silently discarded. This also
  un-breaks the bundled benchmark, which emits `start_ns`/`end_ns`.
- The documented search filters work again: `attr.KEY`, `min_duration_ms`,
  `since`/`until`, and the default `limit` of 100.
- `/v1/stats` exposes the documented `span_count`, `segment_count`, and
  `bytes_on_disk` keys alongside the engine's finer-grained fields.
- The server binds `0.0.0.0` again by default; `--host` overrides.
- TTL expiration no longer empties the in-memory segment set when a file
  operation fails mid-compaction; the store keeps serving its previous view
  and surfaces the error.
- Stale-lock reclamation is single-winner: a reclamation sentinel closes the
  window in which a slow reclaimer could delete a fresh lock and defeat the
  single-writer guarantee.
- The README architecture section describes the engine as built (JSON-lines
  segments, memory-resident, linear scans, no manifest or per-segment index
  yet) instead of the roadmap design, and names the compaction
  crash-atomicity bound in Limitations.

## [0.2.0] - 2026-07-22

### Changed
- `traza-server` is engine-backed: the segment engine is the server's only
  datastore. The HTTP wire contract is unchanged; the server-side append-only
  log, in-memory indexes, and startup replay are removed, and restart
  durability and crash recovery are the engine's.

### Added
- `POST /v1/flush` forces buffered spans into a durable segment on demand.
- `--flush-spans` server flag to tune the engine's flush threshold; `--port 0`
  binds an ephemeral port announced on stderr.
- Five process-level `server_on_engine` integration tests that drive the real
  server binary end to end, including an engine-authority cross-check and a
  kill-and-restart persistence test.
- Stale-lock reclamation: `Store::open` reclaims a lock file whose recorded
  owner process is verifiably dead, so a crashed server cannot permanently
  wedge its data directory. A live owner still rejects the open.

### Fixed
- Deadlock-capable lock-order inversion between `flush()` and `stats()`; all operations now follow a documented writer-before-segments discipline.
- `query()` and `get_trace()` now take an atomic combined snapshot of buffered and persisted spans, so a concurrent flush can no longer hide committed spans.
- Crash-orphaned segment temp files no longer wedge subsequent flushes: temp names are unique per process, and `Store::open` removes orphans during recovery.
- Concurrent writers are rejected: `Store::open` holds a lock file for the store's lifetime and a second open fails with `Error::AlreadyOpen`.

### Added
- Concurrency and failure-injection tests: deadlock detection, read-during-flush consistency (exactly-once), stale-temp recovery, and second-open rejection. Direct per-segment ordering assertion in the flush test.

- Renamed the project and crate to Traza (`traza`).

## [0.1.0] - 2026-07-20

### Added

- The tracing storage engine exposed as a Rust library.
- The `traza-server` HTTP server for the documented ingestion and query endpoints.
- The `bench` executable for measuring the existing datastore workloads.
- Four behavioral integration tests: buffer-flush persistence, crash recovery via reopen, randomized filter equivalence against an independent naive reference, and TTL compaction. (Persisted batch *ordering* is asserted only indirectly; a direct segment-order assertion arrives with the storage-correctness work.)
- Crate documentation, dual MIT/Apache-2.0 licensing, and release automation.

### Known Limitations

- This is an initial 0.1 release; consult README.md for the currently documented operational constraints and unsupported use cases.

[Unreleased]: https://github.com/toshish/traza/compare/v0.19.0...HEAD
[0.19.0]: https://github.com/toshish/traza/releases/tag/v0.19.0
[0.1.0]: https://github.com/toshish/traza/releases/tag/v0.1.0
