# Monitoring

Two endpoints. `GET /v1/metrics` is the Prometheus scrape; `GET /v1/stats` is
the store's own summary and the natural health check.

Both are gated by authentication when it is configured, and both need only a
`ro` token.

## `GET /v1/metrics`

Prometheus text exposition format (`Content-Type: text/plain; version=0.0.4`).
Engine stages first, then the HTTP layer, so one scrape shows the whole ingest
path.

```
# TYPE traza_spans_admitted_total counter
traza_spans_admitted_total 4
# TYPE traza_wal_fsync_ns_count counter
traza_wal_fsync_ns_count 4
…
```

Metrics are **per store**, not global. A process holding several stores gets
separate numbers rather than one meaningless total.

### How accurate the percentiles are

Every stage below is exposed as `_ns_count`, `_ns_sum`, `_ns_max`, `_ns_p50`,
`_ns_p95`, and `_ns_p99`.

**A percentile gauge is the upper bound of the bucket the true value falls in:
at most 1/16 (6.25%) high, never low.** Latencies land in log-linear buckets —
each power of two split into sixteen even steps — so a p95 of `2818047` means
"somewhere in (2.64 ms, 2.82 ms]". `_ns_count`, `_ns_sum` and `_ns_max` are
exact; only the percentiles are bucketed.

That bound is small enough to publish, and the dashboard's Server screen does
publish it — stating the bound alongside the figures rather than implying an
exactness the buckets do not have. `GET /v1/metrics.json` carries it as
`percentile_error_bound`.

> **This changed.** These buckets used to be plain powers of two, which made a
> reported percentile up to **2x** the truth — fine for ranking stages against
> each other, useless on a screen, and this guide said so. Sixteen sub-buckets
> per octave costs one extra shift on the record path and 8 KiB per histogram,
> which is the right trade for numbers a person is going to read.

They are still deliberately **not** exposed as Prometheus histograms with `le`
buckets: a `histogram_quantile` over these bounds would interpolate inside
buckets whose edges are not where the interpolation assumes.

For an exact end-to-end figure, the benchmarks still measure from the client
with a plain `Instant` — see [benchmarking](../internals/benchmarking.md).

### Engine counters

| Metric | Type | Meaning |
|---|---|---|
| `traza_spans_admitted_total` | counter | Spans accepted through the ingest surfaces |
| `traza_batches_admitted_total` | counter | Batches accepted. `spans_admitted / batches_admitted` is the **mean batch size actually reaching the engine** — often more informative than either alone |
| `traza_wal_commits_total` | counter | Calls to commit, whether or not they performed their own fsync. Divided by `traza_wal_fsync_ns_count` this is the **group-commit ratio**: how many acknowledgements each fsync covered |
| `traza_segment_seal_spans_total` | counter | Spans written out by seals, for seal cost per span |

### Engine stages

Each is timed **per batch**, not per span, so the instrumentation sits far below
the noise floor of what it measures.

| Stage | What it times |
|---|---|
| `traza_writer_lock_wait` | Waiting to acquire the writer lock — **the contention signal** |
| `traza_wal_encode` | Encoding a batch into its log frame. Measured deliberately *outside* the writer lock |
| `traza_wal_write` | Appending the frame to the log: the log's own lock plus the write (inside the writer lock) |
| `traza_wal_lock_wait` | The log-lock half of `wal_write` |
| `traza_wal_write_syscall` | The `write` half of `wal_write`, with the log lock already held |
| `traza_wal_fsync` | The fsync. The one stage that is not CPU |
| `traza_buffer_upsert` | Upserting a batch into the write buffer (inside the writer lock) |
| `traza_segment_seal` | Sealing the write buffer into a segment, end to end |
| `traza_segment_seal_locked` | The part of that seal which holds an engine lock — draining, publishing, reconciling. Everything else runs with nothing held |

**Reading `writer_lock_wait` correctly.** It is not a cost, it is the **queue**
for everything else. If it dominates, the fix is to do *less work while holding
the lock*, not to make that work faster. The work performed under the lock is
`wal_write + buffer_upsert + segment_seal_locked`; comparing that sum against
wall clock tells you how saturated the lock is.

**`segment_seal` is not in that sum, and that is the point.** A seal does its
I/O with no lock held, so its wall time is not lock occupancy — use
`segment_seal_locked` for saturation and `segment_seal` for what a seal costs.
If `segment_seal_locked / segment_seal` climbs toward 1, something has put the
segment write back under the lock. Expect it well under 0.25.

`traza_segment_seals_coalesced_total` counts seals that found another already
running and declined to start a second one; those spans are covered by the
running seal. A high number alongside a write buffer above `--flush-spans` just
means seals are back-to-back, which is normal under saturation.

### HTTP layer

| Metric | Type | Meaning |
|---|---|---|
| `traza_http_requests_total` | counter | Requests served |
| `traza_http_rejected_total` | counter | Requests rejected by the auth gate (401 or 403) |
| `traza_http_connections_accepted_total` | counter | Connections accepted |
| `traza_http_connections_refused_total` | counter | Connections refused at `--max-connections` with a `503` |
| `traza_http_connections_live` | gauge | Connections currently open |
| `traza_http_decoded_spans_total` | counter | Spans decoded from request bodies |
| `traza_http_responses_{2xx,4xx,5xx}_total` | counter | Responses by status class |
| `traza_uptime_seconds` | gauge | Seconds since this process began serving |
| `traza_http_decode_ns_{count,sum,max}` | — | Wire decode. For OTLP protobuf this covers the wire decode **and** the OTLP-to-span mapping, which is the whole cost of accepting a batch on that route |
| `traza_http_request_ns_{count,sum,max,p50,p95,p99}` | — | End-to-end request handling, every route together |
| `traza_http_{ingest,lookup,search,stats,other}_ns_{count,sum,p50,p95,p99}` | — | The same, split by route class |

**Request latency is also split by route class**, because one blended
histogram over ingest and search described neither: an ingest batch and a
trace lookup differ by orders of magnitude. The classes are:

| Class | Routes |
|---|---|
| `ingest` | `POST /v1/spans`, `POST /v1/traces` |
| `search` | `GET /v1/spans`, `GET /v1/export` |
| `lookup` | `GET /v1/traces/…`, `/v1/sessions/…`, `/v1/payloads/…`, `/v1/annotations` |
| `stats` | `GET /v1/stats*`, `GET /v1/sessions` |
| `other` | dashboard assets, `/v1/metrics`, `/v1/flush` |

`traza_http_decode` still exposes count, sum and max only. Everything else
above carries percentiles under the accuracy bound stated at the top of this
page. [`GET /v1/metrics.json`](../guide/http-api.md#get-v1metricsjson) returns
all of it as JSON, including that bound.

## `GET /v1/stats`

```json
{"buffered_records":1,"bytes_on_disk":0,"durability":"wal","persisted_records":0,"record_count":1,"segment_count":0,"total_records":1,"wal_bytes":261}
```

| Field | Meaning |
|---|---|
| `buffered_records` | Primary-key-unique records in the in-memory write buffer |
| `persisted_records` | **Physical** records in segments |
| `total_records` | `buffered_records + persisted_records` |
| `record_count` | The same value as `total_records` |
| `segment_count` | Persisted segment files |
| `bytes_on_disk` | Total size of segment files |
| `durability` | `buffered`, `wal`, or `flushed` |
| `wal_bytes` | Bytes the write-ahead log holds — the work a restart would replay |

**`persisted_records` counts physical records, including historical versions
superseded by last-write-wins reads.** Re-ingesting a span hides the old version
from every query immediately, but its bytes stay until compaction rewrites the
segment. So this is an upper bound on the number of distinct spans, not a count
of them. It is defined that way deliberately: the call stays
O(number of segments) instead of decoding the corpus.

If you re-ingest heavily, expect `persisted_records` to exceed your logical
span count and to fall when compaction runs. That is working as intended.

`GET /v1/stats` is cheap and fails if the store cannot be read, which makes it
a good liveness and readiness probe.

## What to alert on

**Ingest has stopped.** `rate(traza_spans_admitted_total[5m]) == 0` while you
expect traffic. This catches a dead exporter, a broken token, and a wedged
store alike.

**Clients are being refused.** Any sustained increase in
`traza_http_connections_refused_total` means you are at `--max-connections`.
Watch `traza_http_connections_live` against the configured limit as the leading
indicator.

**Authentication is failing.** A jump in `traza_http_rejected_total` is either
a misconfigured client or someone probing. It does not distinguish 401 from
403.

**Disk growth.** `bytes_on_disk` against the volume's capacity. Remember that
superseded versions persist until compaction, and that a merge needs room for
its output before its inputs are removed.

**Restart replay is growing.** `wal_bytes` is bounded by `--flush-wal-bytes`
(64 MiB by default), not by `--flush-spans`, and under sustained ingest it runs
up to that bound between reclamations rather than emptying at every seal — a
seal only discards the whole log when it leaves the buffer empty. So a
`wal_bytes` that oscillates below the bound is healthy; one that sits *at* the
bound means every seal is reclaiming, which is the case worth investigating.
On a quiet store it should still fall to zero. If `wal_bytes` exceeds the bound
persistently, check that `--flush-spans` is reachable for your traffic shape.

**Segment count is climbing without bound.** `segment_count` rising steadily is
the signature of compaction being off or unable to keep up, and it is what
makes filtered search slow down over time. It also costs one file descriptor
per segment.

**fsync has become the bottleneck.** Compare `traza_wal_fsync_ns_sum` against
wall-clock time, and watch the group-commit ratio
(`traza_wal_commits_total / traza_wal_fsync_ns_count`). A ratio near 1 under
concurrent load means batches are not coalescing;
[`--wal-commit-window-us`](../configuration.md) is the lever, at the cost of
added acknowledgement latency.

**The writer lock is saturated.** Sum
`traza_wal_write_ns_sum + traza_buffer_upsert_ns_sum +
traza_segment_seal_locked_ns_sum` and compare it against wall clock. Sealing
used to be ~74% of that sum and is now a small part of it, so a saturated lock
today points at `wal_write` — which means the log device, not the engine. The
decomposition and how it moved are in
[`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md#the-limiting-stage).

### What not to alert on

- **The `_p50` / `_p99` stage gauges as SLO latency.** They are bucketed upper
  bounds, and their step changes are bucket boundaries, not real transitions.
  Alerting on them will page you for a value that moved from 4.1 ms to 4.2 ms
  and crossed a power of two.
- **`persisted_records` as a span count.** See above.
- **`traza_writer_lock_wait` in isolation.** High wait with high throughput is a
  busy server, not a broken one.

## Logs

`traza-server` writes to stderr, sparingly: the listening address, the
durability contract, the dashboard directory (or every path searched), and
failures from the background maintenance thread (`segment compaction failed:`,
`expiry compaction failed:`, `export failed after N rows:`). There is no
request log — if you need one, the reverse proxy in front is the right place.

Maintenance failures are worth surfacing. They do not stop the server, and a
compaction that keeps failing shows up first as a climbing `segment_count`.
