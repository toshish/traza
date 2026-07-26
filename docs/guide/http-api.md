# HTTP API reference

Every route Traza serves. Responses are JSON unless noted. All examples were
run against a live server; the response bodies shown are real output.

## Conventions

**Base URL.** `http://HOST:PORT`, default `http://127.0.0.1:8080`. TLS is
reverse-proxy territory — Traza speaks plain HTTP.

**Authentication.** When `TRAZA_TOKENS` is set, every `/v1` request needs
`Authorization: Bearer TOKEN`. A `ro` token may `GET`; an `rw` token may `GET`
and `POST`. See [administration](../operations/administration.md#authentication).

**Timestamps** are integer Unix nanoseconds everywhere, in both directions.

**Unknown query parameters are rejected**, not ignored:
`400 {"error":"unknown query parameter: bogus"}`. A typo fails loudly rather
than silently returning the wrong rows.

**Connections persist.** HTTP/1.1 keep-alive is the default. `GET /v1/export`
is the one route that always closes its connection, because it is chunked with
trailers and has no declared length.

**Limits.** Request bodies are capped at 64 MiB, request headers at 64 KiB.

### Status codes

| Code | Meaning |
|---|---|
| `200` | Success |
| `400` | Malformed body, invalid parameter, or a violated ingest invariant |
| `401` | Missing, malformed, or unknown bearer token. Carries `WWW-Authenticate: Bearer` |
| `403` | Valid token without the scope for this method |
| `404` | No such route, or the requested trace/session/payload does not exist |
| `503` | The store could not serve the request, or the server is at its connection limit. Retry with backoff |

## Route index

| Method | Path | Purpose |
|---|---|---|
| `POST` | [`/v1/spans`](#post-v1spans) | Ingest a native JSON span batch |
| `POST` | [`/v1/traces`](#post-v1traces) | Ingest OTLP/HTTP (protobuf or JSON) |
| `POST` | [`/v1/flush`](#post-v1flush) | Seal buffered spans into a segment |
| `GET` | [`/v1/spans`](#get-v1spans) | Filtered span search |
| `GET` | [`/v1/traces/{trace_id}`](#get-v1tracestrace_id) | One trace's spans and annotations |
| `GET` | [`/v1/sessions`](#get-v1sessions) | Sessions, most recent activity first |
| `GET` | [`/v1/sessions/{id}`](#get-v1sessionsid) | One session's rollup and per-trace breakdown |
| `GET` | [`/v1/stats/llm`](#get-v1statsllm) | Token and cost aggregation |
| `GET` | [`/v1/stats/series`](#get-v1statsseries) | Volume, errors, tokens, cost and latency over time |
| `GET` | [`/v1/stats/duration`](#get-v1statsduration) | Duration distribution and percentiles |
| `GET` | [`/v1/stats/failures`](#get-v1statsfailures) | Errors grouped by signature |
| `GET` | [`/v1/stats/slowest`](#get-v1statsslowest) | The slowest matching spans |
| `POST` | [`/v1/annotations`](#post-v1annotations) | Attach a score or feedback record |
| `GET` | [`/v1/annotations`](#get-v1annotations) | Query annotations |
| `GET` | [`/v1/payloads/{reference}`](#get-v1payloadsreference) | Raw bytes of an offloaded payload |
| `GET` | [`/v1/export`](#get-v1export) | Streaming NDJSON export |
| `GET` | [`/v1/stats`](#get-v1stats) | Store statistics |
| `GET` | [`/v1/metrics`](#get-v1metrics) | Prometheus text metrics |
| `GET` | [`/v1/metrics.json`](#get-v1metricsjson) | The same metrics as JSON |
| `GET` | [`/`, `/dashboard`](#get--and-dashboard) | The trace browser |

---

## Ingest

### `POST /v1/spans`

Ingests a batch of spans in Traza's native JSON.

**Body.** Either a bare array of spans or `{"spans": [...]}`. Field semantics
are in the [data model](data-model.md#the-span).

**Response `200`.**

```json
{"accepted":1,"durability":"wal"}
```

`accepted` is the number of spans in the batch. `durability` is `buffered`,
`wal`, or `flushed` and states what the acknowledgement guarantees.

**Errors.**

| Response | Cause |
|---|---|
| `400 {"error":"body must be an array or {spans: [...]}"}` | Body's first non-whitespace byte is neither `[` nor `{` |
| `400 {"error":"missing field \`name\` at line 1 column 31"}` | A required field is absent; serde names it |
| `400 {"error":"span 0: trace_id is empty"}` | Empty primary-key half, at batch index 0 |
| `400 {"error":"span 0: span_id is empty"}` | As above |
| `503 {"error":"…"}` | The store rejected the write |

Validation is atomic per batch: one invalid span stores none of them.

### `POST /v1/traces`

OTLP/HTTP ingest. `Content-Type: application/x-protobuf` selects the binary
decoder; anything else is parsed as OTLP/HTTP JSON.

**Response `200` (JSON request).**

```json
{"partialSuccess":{}}
```

**Response `200` (protobuf request).** `Content-Type: application/x-protobuf`
with a zero-length body — the encoding of an empty `ExportTraceServiceResponse`.

**Errors.** `400` with the decode failure for malformed protobuf or JSON, a
non-hex id, or a timestamp that is not a `u64`. `503` if the store rejected the
write.

The full field mapping is in [ingest](ingest.md#otlp-mapping).

### `POST /v1/flush`

Seals every currently buffered span into a durable segment.

```json
{"flushed":true}
```

Errors: `503` if the seal failed.

---

## Query

### `GET /v1/spans`

Filtered span search, ordered by Traza's stable span order
(`start_time_ns`, `end_time_ns`, `trace_id`, `span_id`).

**Parameters.** All filters are ANDed. All values are URL-encoded.

| Parameter | Type | Description |
|---|---|---|
| `service` | string | Exact match on the emitting service |
| `name` | string | Exact match on the operation name |
| `status` | string | Exact match on the span's own `status` field |
| `not_status` | string | Exclude spans with this status. Repeatable |
| `attr.KEY` | JSON or string | Exact attribute match. Repeatable — each occurrence adds a condition |
| `content` / `q` | string | Spans whose text contains **every word** given. See below |
| `session` | string | Every span of a session, unioning all recognized session keys |
| `cursor` | string | Opaque token from a previous response's `next_cursor`. Returns the page after it |
| `not_attr.KEY` | JSON or string | Exclude spans whose attribute equals this. A span missing the key entirely is **kept**. Repeatable |
| `min_attr.KEY` | number | Attribute is at least this, compared numerically. Repeatable |
| `max_attr.KEY` | number | Attribute is at most this, compared numerically. Repeatable |
| `min_duration_ms` | integer | Minimum `end - start`, in milliseconds |
| `min_duration_ns` | integer | Minimum `end - start`, in nanoseconds |
| `max_duration_ms` | integer | Maximum `end - start`, in milliseconds |
| `max_duration_ns` | integer | Maximum `end - start`, in nanoseconds |
| `since` / `since_ns` | integer | Only spans starting at or after this Unix-nanosecond timestamp |
| `until` / `until_ns` | integer | Only spans starting at or before this timestamp |
| `sort` | string | `duration` / `-duration` / `start` / `-start` (a leading `-` is descending). Omit for Traza's stable order |
| `limit` | integer | Maximum spans returned. **Default 100**, applied after filtering |

#### Content search: `content=`

`content=refund` returns spans whose text contains the word *refund*. The text
searched is every string value in the span's attributes and its events'
attributes, plus event names — so a prompt, a completion, a tool call's
arguments and a nested message array are all covered.

```
GET /v1/spans?content=refund&since=1700000000000000000&limit=50
```

**It is word search, not substring search, and not a phrase search.** Those
three differences are the ones that surprise people, so they are worth stating
directly:

| Query | Matches `Refund the order` | Matches `refunds were issued` |
|---|---|---|
| `content=refund` | yes | **no** |
| `content=refunds` | no | yes |
| `content=refund order` | yes (both words present) | no |

- Case and punctuation are ignored: `refund`, `Refund`, `"refund"` and
  `refund,` are the same word.
- There is no stemming. `refund` and `refunds` are different words.
- A multi-word query is a **conjunction**: every word must appear somewhere in
  the span, in any order. `content=refund order` does not require them
  adjacent.
- Words are runs of ASCII letters and digits. Text in other scripts is not
  tokenized, so `content=世界` matches nothing.
- **An offloaded value is searchable only within its 256-character preview.**
  A string attribute longer than `--payload-threshold-bytes` is moved to the
  payload store at ingest — before anything indexes the span — and replaced
  inline by a reference carrying a preview. Only that preview is searchable.
  At the 256 KiB default almost nothing is offloaded; lower the threshold and
  this becomes the rule rather than the exception. Search is bounded here, not
  wrong: the index and the match are computed from the same text, so a span is
  never skipped that would have matched.

This is not an arbitrary restriction — it is what the index can answer
*correctly*. A word index cannot soundly drive a substring search: it would
have to skip the span reading `refunds were issued` when you searched
`refund`, and skipping it is a wrong answer rather than a slow one. See
[the segment format](../segment-format.md#the-content-index).

To find spans by the *whole value* of an attribute rather than a word inside
it, use `attr.KEY=VALUE`, which is exact and separately indexed.

Content search composes with every other parameter, and is fast when it is
selective — see [capacity](../operations/capacity.md#content-search) for
measured latencies, including the case where it cannot help.

**`status=` reads the span's own field; `attr.status=` reads an attribute.**
These are different filters and the difference used to be a trap: every
aggregate in the store counts errors from `Span::status`, but nothing could
*select* on it, so the natural-looking `attr.status=error` matched an attribute
most instrumentation never writes and returned an empty array indistinguishable
from "no errors". Use `status=error` for failures and `not_status=error` for
their complement. `not_status` has no missing-key subtlety — every span has a
status, and an empty one is a value like any other.

`attr.KEY=VALUE` **matches a scalar regardless of how it was typed on the
wire.** `attr.code=200` finds spans that stored the number `200` and spans
that stored the string `"200"`. This matters because instrumentation is
inconsistent about it — several SDKs stringify token counts and status codes —
and the previous behaviour matched only the number, so a store full of
stringified values answered every such query with an empty array that was
indistinguishable from "no such data". Matching is still exact: `200` does not
match `20`, and containers (arrays, objects) compare structurally.

`min_attr.KEY` / `max_attr.KEY` read the attribute as a number whether it was
stored as one or as a string, so `min_attr.llm.usage.total_tokens=1000` works
across both conventions. A span whose attribute is absent or non-numeric does
not match a numeric bound.

`not_attr.KEY` deliberately **keeps** spans that lack the key:
`not_attr.status=error` means "not known to be an error", which includes spans
that never recorded a status. Treating a missing key as excluded would hide
most of a corpus behind a filter that reads like it only removes failures.

**Sorting costs a full scan.** Without `sort`, a query stops as soon as it has
`limit` matches. With it, every match must be found before any can be ranked,
so `sort` is refused with `400` past an internal candidate ceiling — narrow
with a time range, `service`, or an attribute and retry. Returning the "ten
slowest" out of an arbitrary first page would be a wrong answer that looks
like a right one.

**Time ranges skip a segment's records.** `since` / `until` are compared
against each segment's stored timestamp range, so a segment that cannot
contain a
match has none of its records read. `traza_segments_pruned_by_time_total` and
`traza_segments_examined_total` in [`/v1/metrics`](../operations/monitoring.md)
show how much of the store a given window is eliminating. Segments written
before v0.18 carry no range and are always scanned; they age out through
normal compaction.

The work avoided is real and counter-verified, but **no latency figure is
published for it yet**: the only measurement attempted so far ran on a
40-segment store, which is too small for per-segment cost to be visible above
noise. Expect it to matter in proportion to segment count, like compaction.

`session=` is not the same as `attr.session.id=`: it unions every recognized
session key, so a session whose spans use mixed conventions returns whole.

**Paging is by cursor, not by growing `limit`.** A response whose page came
back full carries `next_cursor`; passing it back returns the spans strictly
after the last one, in the same total order. Re-requesting with a larger
`limit` instead re-reads and re-sends every row already in hand, which makes
scrolling quadratic in the number of pages. A short page carries
`next_cursor: null` — it has already reached the end, and a cursor there could
only return nothing. The token is opaque; a malformed one is a `400` rather
than a silently wrong page.

**Response `200`.** An envelope: the spans, the cursor, and what the query
cost.

```sh
curl 'http://localhost:8080/v1/spans?service=checkout&attr.region=us-east&min_duration_ms=2&limit=50'

# The ten slowest failed calls in a window that cost over a cent
curl 'http://localhost:8080/v1/spans?since_ns=1700000000000000000&until_ns=1700003600000000000&status=error&min_attr.llm.cost_usd=0.01&sort=-duration&limit=10'
```

```json
{"spans":[{"attributes":{"http.method":"POST","region":"us-east"},"end_time_ns":1700000000002500000,"events":[],"name":"charge","parent_span_id":null,"service":"checkout","span_id":"span-1","start_time_ns":1700000000000000000,"status":"ok","trace_id":"trace-1"}],"next_cursor":null,"cost":{"elapsed_ns":91375,"segments_examined":12,"segments_pruned":11}}
```

| Field | Type | Description |
|---|---|---|
| `spans` | array | The matching spans, in Traza's stable order (or `sort` order) |
| `next_cursor` | string or null | Pass as `cursor=` for the next page; `null` at the end |
| `cost.elapsed_ns` | integer | Time inside the engine, excluding serialization |
| `cost.segments_examined` | integer | Segments the query considered |
| `cost.segments_pruned` | integer | Segments skipped whole because their timestamp range could not hold a match |

`cost` is counted per query rather than sampled from the process-wide
counters, which race under concurrent readers and cannot be attributed. It is
what lets a caller — or the dashboard — state how much of the store a window
actually eliminated instead of asserting that filtering is cheap.

An empty result is `{"spans":[],...}`, not a `404`.

**Errors.** `400` for an unknown parameter or an unparseable numeric value
(`{"error":"invalid limit"}`, `{"error":"invalid min_duration_ms"}`,
`{"error":"invalid since"}`, `{"error":"invalid until"}`). `503` on store
failure.

### `GET /v1/traces/{trace_id}`

Every span carrying `trace_id`, ordered by start time, plus every annotation
attached to that trace. The id is percent-decoded from the path.

```sh
curl http://localhost:8080/v1/traces/trace-1
```

```json
{"annotations":[],"spans":[{"attributes":{"http.method":"POST","region":"us-east"},"end_time_ns":1700000000002500000,"events":[],"name":"charge","parent_span_id":null,"service":"checkout","span_id":"span-1","start_time_ns":1700000000000000000,"status":"ok","trace_id":"trace-1"}],"trace_id":"trace-1"}
```

| Field | Type | Description |
|---|---|---|
| `trace_id` | string | The requested id |
| `spans` | array | The trace's spans, ordered by start time |
| `annotations` | array | Annotations on the trace or any of its spans |

**Errors.** `404 {"error":"trace not found"}` when no span carries the id.
`503` on store failure.

### `GET /v1/export`

Streams matching spans as chunked NDJSON — one span per line — with bounded
memory. An export larger than RAM is fine.

**Parameters.** Identical to [`GET /v1/spans`](#get-v1spans), with one
difference: **exports are unbounded by default.** Passing `limit` explicitly
caps the stream.

**Response `200`.**

```
Content-Type: application/x-ndjson
Transfer-Encoding: chunked
Trailer: X-Traza-Export-Complete, X-Traza-Export-Count
Connection: close
```

The body is one JSON span per line. The stream ends with HTTP trailers:

```
X-Traza-Export-Complete: true
X-Traza-Export-Count: 1
```

**Programmatic clients must check `X-Traza-Export-Complete`.** A `false` value
means the stream failed *after* its `200` response had begun — the only way to
signal that, since the status line is long gone. `X-Traza-Export-Count` is the
number of rows actually emitted and can be cross-checked.

**An export is a snapshot.** The store is pinned when the export starts, and
every page comes from that one state. Spans ingested, replaced, or expired
while the stream is running do not appear and do not change what does: the
output is exactly the dataset that existed at the first byte, with each
primary key appearing at most once. `complete: true` therefore means "this is
that whole dataset", not merely "the connection ended tidily".

A pinned export holds the segment files it is reading, so compaction and
retention cannot reclaim their disk space until it finishes. Exporting a large
store over a slow connection delays reclamation for as long as it runs.

This route always closes its connection.

```sh
curl 'http://localhost:8080/v1/export?service=support-agent' > dataset.ndjson
```

---

## Sessions and analytics

### `GET /v1/sessions`

Sessions active in the window, most recent activity first.

| Parameter | Type | Description |
|---|---|---|
| `since` / `since_ns` | integer | Window start, Unix nanoseconds |
| `until` / `until_ns` | integer | Window end, Unix nanoseconds |
| `limit` | integer | Maximum sessions returned. Default **100** |

`group_by` is explicitly rejected here: `400 {"error":"group_by is not a
/v1/sessions parameter"}`.

**Response `200`.**

```json
{"sessions":[{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"llm_calls":1,"prompt_tokens":412,"session_attribute":"gen_ai.conversation.id","session_id":"chat-4711","span_count":1,"total_tokens":500,"trace_count":1}]}
```

| Field | Type | Description |
|---|---|---|
| `session_id` | string | The identifier shared by the session's spans |
| `session_attribute` | string | Which recognized key grouped this session |
| `first_start_ns` / `last_end_ns` | integer | Activity window |
| `trace_count` | integer | Distinct traces containing session spans |
| `span_count` | integer | Spans carrying this session id |
| `llm_calls` | integer | Spans recognized as LLM calls |
| `prompt_tokens` / `completion_tokens` / `total_tokens` | integer | Token sums |
| `cost_usd` | number | Summed cost, when ingest supplied one |
| `error_count` | integer | Spans with status `error` |

**Errors.** `400` for an unknown parameter or unparseable number. `503` on
store failure.

### `GET /v1/sessions/{id}`

One session's rollup plus its per-trace breakdown. The id is percent-decoded
and resolved under any recognized session key.

**Response `200`.** Every field of a session summary, at the top level, plus
`traces`:

```json
{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"llm_calls":1,"prompt_tokens":412,"session_attribute":"gen_ai.conversation.id","session_id":"chat-4711","span_count":1,"total_tokens":500,"trace_count":1,"traces":[{"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"root_name":"openai.chat","span_count":1,"total_tokens":500,"trace_id":"trace-2"}]}
```

Each entry of `traces` carries `trace_id`, `root_name` (the name of the
trace's earliest session span), `first_start_ns`, `last_end_ns`, `span_count`,
`total_tokens`, `cost_usd`, and `error_count`, ordered by first activity.

**Errors.** `404 {"error":"session not found"}`. `503` on store failure.

### `GET /v1/stats/llm`

Token and cost aggregation, sorted by cost, then tokens, then key.

| Parameter | Type | Description |
|---|---|---|
| `group_by` | enum | `model`, `provider`, `service`, `session`, or `day`. Default `model` |
| `since` / `since_ns` | integer | Window start, Unix nanoseconds |
| `until` / `until_ns` | integer | Window end, Unix nanoseconds |
| `limit` | integer | Truncates the row list. Unbounded by default |

**Response `200`.**

```json
{"rows":[{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"key":"gpt-4o","llm_calls":1,"llm_duration_ns":900000000,"prompt_tokens":412,"spans":1,"total_tokens":500}]}
```

| Field | Type | Description |
|---|---|---|
| `key` | string | Model name, provider, service, session id, or `YYYY-MM-DD` UTC day |
| `spans` | integer | All spans in the group |
| `llm_calls` | integer | Spans recognized as LLM calls |
| `prompt_tokens` / `completion_tokens` / `total_tokens` | integer | Token sums |
| `cost_usd` | number | Summed cost |
| `error_count` | integer | Spans with status `error` |
| `llm_duration_ns` | integer | Summed LLM-call duration; divide by `llm_calls` for average latency |

Aggregates are exact. Sealed segments contribute cached rollups — segments are
immutable, so a rollup is computed once — and window edges fall back to
decoding only the boundary segments. Integer counters saturate at `u64::MAX`
rather than wrapping; non-finite cost values are ignored.

**Errors.** `400 {"error":"group_by must be model|provider|service|session|day"}`
for an unrecognized dimension; `400` for an unknown parameter. `503` on store
failure.

**The aggregation routes below do not block ingest.** Each folds the match set
in one pass and constant memory, against a snapshot taken up front — one copy
of the bounded write buffer and one reference per segment — so a scan of the
whole corpus runs alongside writes rather than stopping them, and reports one
coherent instant rather than a moving one.

### `GET /v1/stats/series`

Buckets matching spans into even time buckets. Accepts every
[`GET /v1/spans`](#get-v1spans) filter, plus:

| Parameter | Type | Description |
|---|---|---|
| `since` / `until` | integer | **Required.** The window, Unix nanoseconds |
| `buckets` | integer | Bucket count, clamped to 1–512. Default 24 |

One pass produces volume, errors, LLM calls, tokens, cost and duration
percentiles together, because every surface that wants one of them wants
several, and separate routes would mean separate scans of the same window.

```sh
curl 'http://localhost:8080/v1/stats/series?since=1700000000000000000&until=1700086400000000000&buckets=24&service=checkout'
```

```json
{"since_ns":1700000000000000000,"until_ns":1700086400000000000,"bucket_ns":3600000000000,"buckets":[{"start_ns":1700000000000000000,"spans":412,"errors":7,"llm_calls":180,"total_tokens":220144,"cost_usd":0.7628,"p50_ns":1811939327,"p95_ns":7784628223}]}
```

`buckets` is always exactly the requested length, so a quiet period is a
visible gap rather than something the caller reconstructs from timestamps.

**Errors.** `400 {"error":"since and until are required for a series"}`;
`400 {"error":"until must be after since"}`; `400` for an unknown parameter.

### `GET /v1/stats/duration`

The duration distribution of matching spans. Accepts every
[`GET /v1/spans`](#get-v1spans) filter.

```sh
curl 'http://localhost:8080/v1/stats/duration?service=checkout&since=1700000000000000000'
```

```json
{"count":1852,"min_ns":1000000,"max_ns":118000000000,"mean_ns":3402118942,"p50_ns":2550136831,"p75_ns":4160749567,"p90_ns":7516192767,"p95_ns":11274289151,"p99_ns":51539607551,"buckets":[{"upper_ns":1048575,"count":3}]}
```

Percentiles are the upper bound of a log-linear bucket: **at most 6.25% high,
never low.** `min_ns`, `max_ns`, `mean_ns` and `count` are exact. Only occupied
buckets are returned — a distribution spanning nanoseconds to minutes fills a
few dozen of the thousand, and the zeros would be almost the whole payload.

### `GET /v1/stats/failures`

Error spans grouped by `(service, name, status)`, most frequent first. Accepts
every [`GET /v1/spans`](#get-v1spans) filter; `limit` caps the group count
(default 100). Without an explicit `status`, the filter defaults to
`status=error`.

```sh
curl 'http://localhost:8080/v1/stats/failures?since=1700000000000000000&limit=20'
```

```json
{"groups":[{"service":"checkout-api","name":"tool.refund_lookup","status":"error","count":142,"first_seen_ns":1700000000000000000,"last_seen_ns":1700003600000000000,"example_trace_id":"trace-9f2c","example_span_id":"span-a","p50_ns":325000000,"p95_ns":812000000}],"total":169,"distinct":7,"groups_omitted":0,"spans_untracked":0}
```

| Field | Type | Description |
|---|---|---|
| `groups` | array | Signatures, most frequent first, truncated to `limit` |
| `total` | integer | **Every** matching span, counted before truncation |
| `distinct` | integer | Signatures seen, up to the cardinality bound |
| `groups_omitted` | integer | Signatures measured but cut by `limit` |
| `spans_untracked` | integer | Spans whose signature was never tracked because the cardinality bound was reached |

**Use `total` as the denominator for a share, not the sum of `groups`.** The
returned page is truncated, so summing it overstates every signature's
fraction of the whole — by more, the more signatures exist.

**Grouping is bounded at 4,096 distinct signatures.** Each tracked signature
costs a duration histogram, so an unbounded map turns high-cardinality error
text — an id or a timestamp in a span name — into gigabytes of server memory
to answer a question whose useful form is twenty rows. Past the bound, spans
are counted into `total` but not grouped, and `spans_untracked` says how many.
A non-zero value means `distinct` is a floor.

Grouping happens server-side because the input can be every error in the
window while the useful answer is a dozen rows: shipping the spans so a client
could group them would move megabytes to fill one screen. `example_trace_id`
is the most recent occurrence, so a group is something you can open.

### `GET /v1/stats/slowest`

The slowest matching spans, ranked across the whole match set rather than
within an arbitrary first page. Accepts every
[`GET /v1/spans`](#get-v1spans) filter; `limit` defaults to 10.

```sh
curl 'http://localhost:8080/v1/stats/slowest?since=1700000000000000000&limit=10'
```

```json
{"spans":[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout","start_time_ns":1700000000000000000,"end_time_ns":1700000118000000000,"status":"ok","attributes":{},"events":[]}]}
```

Unlike `sort=-duration` on [`GET /v1/spans`](#get-v1spans), this route has no
candidate ceiling: it keeps only `limit` spans while folding, so memory is
bounded by the answer rather than by the match set. `limit` is itself capped at
1,000 — a tail is read, not paged, and a larger request is a way to ask the
server to hold the match set in memory.

---

## Annotations

### `POST /v1/annotations`

Records one annotation against a span or a whole trace, without mutating the
span.

**Body.**

| Field | Type | Required | Description |
|---|---|---|---|
| `trace_id` | string | yes | Trace containing the annotated span |
| `span_id` | string | no | Annotated span; an empty or absent value annotates the trace as a whole |
| `name` | string | yes | For example `quality`, `thumbs`, `groundedness` |
| `value` | any JSON | yes | Number, string, boolean, object |
| `source` | string | no | Convention: `human:<who>` or `eval:<evaluator>` |
| `comment` | string | no | Free-form |
| `timestamp_ns` | integer | no | Unix nanoseconds. **Absent or `0` is filled in with the server's current time** |

```sh
curl -X POST http://localhost:8080/v1/annotations \
  -H 'Content-Type: application/json' \
  -d '{"trace_id":"trace-2","span_id":"span-a","name":"groundedness",
       "value":0.9,"source":"eval:nightly"}'
```

```json
{"recorded":true}
```

**Errors.** `400` with the parse error for a malformed body; `400` when the
engine rejects the annotation as invalid; `503` on store failure.

### `GET /v1/annotations`

Every supplied narrowing must match. **All of them are optional** — an empty
query returns every annotation, newest first.

| Parameter | Type | Description |
|---|---|---|
| `trace_id` | string | Narrow to one trace |
| `span_id` | string | Narrow to one span |
| `name` | string | Narrow to one annotation name |
| `source` | string | Sources **starting with** this, so `human:` and `eval:` each select a family |
| `since` / `since_ns` | integer | Recorded at or after this timestamp |
| `until` / `until_ns` | integer | Recorded at or before this timestamp |
| `limit` | integer | Maximum returned, applied after ordering |

`trace_id` used to be required, which made an eval run unreadable: its scores
exist per trace, but a run is a population and nobody wants it one trace at a
time. Annotations are indexed in memory at human/eval scale, so the
cross-trace path is a scan of that index rather than anything touching a
segment.

Results are newest first, tie-broken by `(trace_id, span_id, name)` so a bulk
import sharing one timestamp still pages deterministically.

```sh
curl 'http://localhost:8080/v1/annotations?trace_id=trace-2'

# Everything a nightly evaluator scored in the last day
curl 'http://localhost:8080/v1/annotations?source=eval:&since=1700000000000000000&limit=500'
```

```json
{"annotations":[{"comment":"","name":"groundedness","source":"eval:nightly","span_id":"span-a","timestamp_ns":1785005352756011000,"trace_id":"trace-2","value":0.9}]}
```

**Errors.** `400` for an unknown parameter or an unparseable number. `503` on
store failure.

---

## Payloads

### `GET /v1/payloads/{reference}`

Returns the raw bytes of an offloaded payload. `{reference}` is the full
`sha256/<hex>` value from a span's `$payload` field, percent-decoded from the
path — the `/` between `sha256` and the hex digest is part of it.

```sh
curl http://localhost:8080/v1/payloads/sha256/29927e273accc68286005017f7fa6e4f27bddb4db3083ff8b8d4c3667905b7fa
```

**Response `200`.** `Content-Type: application/octet-stream` with the payload
bytes. Traza does not record the original media type; interpret it using the
attribute the reference came from.

**Errors.** `404 {"error":"payload not found"}`. `503` on store failure.

---

## Operations

### `GET /v1/stats`

```json
{"buffered_records":1,"bytes_on_disk":0,"durability":"wal","persisted_records":0,"record_count":1,"segment_count":0,"total_records":1,"wal_bytes":261}
```

| Field | Type | Description |
|---|---|---|
| `buffered_records` | integer | Primary-key-unique records in the in-memory write buffer |
| `persisted_records` | integer | **Physical** records in segments, including historical versions superseded by last-write-wins |
| `total_records` | integer | `buffered_records + persisted_records` |
| `record_count` | integer | Same value as `total_records` |
| `segment_count` | integer | Persisted segment files |
| `bytes_on_disk` | integer | Total size of segment files |
| `durability` | string | `buffered`, `wal`, or `flushed` |
| `wal_bytes` | integer | Bytes the write-ahead log holds — the work a restart would replay. Zero in `buffered` mode and immediately after a flush |

`persisted_records` counts physical records so that the call stays
O(number of segments) instead of decoding the corpus. It is therefore an upper
bound on the number of distinct spans, not the count of them. See
[monitoring](../operations/monitoring.md#get-v1stats).

**Errors.** `503` on store failure.

### `GET /v1/metrics`

Prometheus text exposition format (`Content-Type: text/plain; version=0.0.4`).
Engine stages first, then the HTTP layer.

```
# TYPE traza_spans_admitted_total counter
traza_spans_admitted_total 4
…
```

Every metric name and its meaning is documented in
[monitoring](../operations/monitoring.md), including the accuracy bound on the
percentile gauges.

**Errors.** None — the route always renders.

### `GET /v1/metrics.json`

The same numbers as [`/v1/metrics`](#get-v1metrics), shaped for a browser.
Prometheus text exposition is for scrapers; asking a dashboard to ship an
exposition parser in order to draw one chart is a parser nobody should have to
write.

```json
{"uptime_ns":535200000000000,"durability":"wal","requests":{"total":184204,"rejected":0,"responses_2xx":184204,"responses_4xx":0,"responses_5xx":0,"mean_ns":1210880,"max_ns":75821375,"p50_ns":1703935,"p95_ns":2818047,"p99_ns":9437183},"by_class":{"search":{"count":8,"mean_ns":22375885,"max_ns":75821375,"p50_ns":1703935,"p95_ns":2818047,"p99_ns":9437183},"lookup":{},"stats":{},"ingest":{},"other":{}},"connections":{"accepted":12,"refused":0,"live":1},"decode":{"spans":2417882,"mean_ns":91375,"p95_ns":131071},"ingest":{"spans_admitted":2417882,"batches_admitted":2418,"wal_commits":2418,"wal_fsync_p95_ns":9879551,"segment_seal_p95_ns":0},"pruning":{"segments_examined":340,"segments_pruned_by_time":328},"percentile_error_bound":0.0625}
```

Request latency is split by **route class** — `ingest`, `lookup`, `search`,
`stats`, `other` — because an ingest batch and a trace lookup differ by orders
of magnitude and one blended histogram described neither. `by_class` carries
`count`, `mean_ns`, `max_ns` and the three percentiles for each.

`durability` is the same value [`GET /v1/stats`](#get-v1stats) reports, carried
here so a client showing server health does not have to infer what an
acknowledged write guarantees — or, as the dashboard once did, default it.

`percentile_error_bound` is stated in the payload rather than left implicit:
every `p*_ns` here is a bucket upper bound, at most that fraction high and
never low. Counts, sums, means and maxima are exact.

**Errors.** None — the route always renders.

### `GET /` and `/dashboard`

Serve the built dashboard's `index.html`; other paths under the build root
serve their assets with the right content type. `/dashboard/` works too.

These routes are served **before** the authentication gate: the page is static
build output carrying no data, and every `/v1` call it makes stays gated. Path
traversal outside the build root is refused.

With no dashboard build available:

```json
{"error":"no dashboard build found","next":"build it with: cd ui && npm ci && npm run build (serving /path/to/ui/dist)"}
```

with status `404`. The API is unaffected.
