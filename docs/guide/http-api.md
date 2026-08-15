# HTTP API reference

Every route Traza serves. Responses are JSON unless noted. All examples were
run against a live server; the response bodies shown are real output.

## Conventions

**Base URL.** `http://HOST:PORT`, default `http://127.0.0.1:8080`. TLS is
reverse-proxy territory — Traza speaks plain HTTP.

**Authentication.** When `TRAZA_TOKENS` is set, every `/v1` request needs
`Authorization: Bearer TOKEN`. Entries are `scope[@tenant]:token`: a `ro` token
may `GET`; an `rw` token may `GET` and `POST`; `admin` adds erasure. The
optional `@tenant` **binds** the credential — a bound credential reads and
writes only its tenant's data, on every surface: search, export, tail,
analytics, annotations, payload fetch, evals, erasure, and MCP alike. The
binding is applied where queries are constructed, so no endpoint can forget
it. Two consequences worth knowing before they surprise you:

- Naming a foreign tenant in any `tenant=` parameter answers
  `403 {"error":"this credential is bound to a different tenant"}` — the
  caller knows what it asked for, so the refusal says why.
- The store-global operator endpoints — `/v1/stats`, `/v1/metrics`,
  `/v1/metrics.json`, `/v1/verify`, `/v1/checkpoint`, `/v1/flush`, and
  `/v1/backups/*` — answer bound credentials `403 {"error":"forbidden"}`:
  they report and move the whole store's data, and even the volumes disclose
  co-tenants. A bound credential's accounting surface is
  [`GET /v1/tenants`](#get-v1tenants).

See [administration](../operations/administration.md#authentication).

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
| `403` | Valid token without the scope for this method, or a tenant-bound credential naming a foreign tenant or a store-global operator endpoint |
| `404` | No such route, or the requested trace/session/payload/dataset/experiment does not exist. Cross-tenant reads answer `404`, never `403` — existence is a fact about another tenant |
| `409` | A conflict a retry may resolve: a mutation for a tenant whose erasure is pending, a tombstoned parent version, a payload reference not in the store, a pin label already taken |
| `410` | A tombstoned dataset version: the content is withheld, the fact of the deletion is not |
| `503` | The store could not serve the request, or the server is at its connection limit. Retry with backoff |

## Route index

| Method | Path | Purpose |
|---|---|---|
| `POST` | [`/v1/spans`](#post-v1spans) | Ingest a native JSON span batch |
| `POST` | [`/v1/traces`](#post-v1traces) | Ingest OTLP/HTTP (protobuf or JSON) |
| `POST` | [`/v1/flush`](#post-v1flush) | Seal buffered spans into a segment |
| `POST` | [`/v1/checkpoint`](#post-v1checkpoint) | Publish a generation |
| `POST` | [`/v1/backups/{label}`](#post-v1backupslabel) | Pin and verify a backup |
| `POST` | [`/v1/backups/{label}/release`](#post-v1backupslabelrelease) | Release a pin |
| `GET` | [`/v1/verify`](#get-v1verify) | Verify the live generation |
| `POST` | [`/v1/erasures`](#post-v1erasures) | Erase a trace, span, session, payload, or whole tenant |
| `GET` | [`/v1/erasures`](#get-v1erasures) | The tombstone log |
| `GET` | [`/v1/erasures/{id}/verify`](#get-v1erasuresidverify) | The erasure receipt |
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
| `POST` | [`/v1/datasets`](#post-v1datasets) | Create a dataset |
| `GET` | [`/v1/datasets`](#get-v1datasets) | List datasets with version summaries |
| `GET` | [`/v1/datasets/{id}`](#get-v1datasetsid) | One dataset with version summaries |
| `POST` | [`/v1/datasets/{id}/versions`](#post-v1datasetsidversions) | Create a content-addressed dataset version |
| `GET` | [`/v1/datasets/{id}/versions/{vid}`](#get-v1datasetsidversionsvid) | One version's manifest and example bodies |
| `POST` | [`/v1/datasets/{id}/versions/{vid}/tombstone`](#post-v1datasetsidversionsvidtombstone) | Logically delete a version (`admin`) |
| `POST` | [`/v1/experiments`](#post-v1experiments) | Create an experiment against a dataset version |
| `GET` | [`/v1/experiments`](#get-v1experiments) | List experiments |
| `GET` | [`/v1/experiments/{id}`](#get-v1experimentsid) | One experiment with derived state |
| `POST` | [`/v1/experiments/{id}/runs`](#post-v1experimentsidruns) | Record a task run |
| `GET` | [`/v1/experiments/{id}/runs`](#get-v1experimentsidruns) | An experiment's recorded runs |
| `GET` | [`/v1/experiments/{id}/scores`](#get-v1experimentsidscores) | An experiment's scores |
| `GET` | [`/v1/experiments/{id}/summary`](#get-v1experimentsidsummary) | Per-name score distributions |
| `GET` | [`/v1/experiments/diff`](#get-v1experimentsdiff) | Compare two experiments' scores |
| `GET` | [`/v1/tenants`](#get-v1tenants) | Per-tenant usage accounting |
| `GET` | [`/v1/payloads/{reference}`](#get-v1payloadsreference) | Raw bytes of an offloaded payload |
| `GET` | [`/v1/export`](#get-v1export) | Streaming NDJSON export |
| `GET` | [`/v1/tail`](#get-v1tail) | Live span stream, in admission order |
| `GET` | [`/v1/stats`](#get-v1stats) | Store statistics |
| `GET` | [`/v1/metrics`](#get-v1metrics) | Prometheus text metrics |
| `GET` | [`/v1/metrics.json`](#get-v1metricsjson) | The same metrics as JSON |
| `POST` | [`/v1/mcp`](#post-v1mcp) | Model Context Protocol endpoint (off unless `--mcp`) |
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

`accepted` is the number of spans **stored** — it is a durability claim, and
it counts nothing else. `durability` is `buffered`, `wal`, or `flushed` and
states what the acknowledgement guarantees. While an
[erasure](#erasure) is pending, spans it covers are acknowledged and
deliberately not stored; the response then carries the split explicitly:

```json
{"accepted":1,"suppressed":1,"durability":"wal"}
```

`suppressed` appears only when nonzero.

**Tenancy.** A span may carry a top-level `$tenant` field: lowercase
`[a-z0-9][a-z0-9._-]`, at most 64 bytes. Empty or absent is the **default
tenant**, and it is never serialized back — a single-tenant deployment writes
byte-identical records to what it wrote before tenancy existed. The tenant is
part of the primary key `(tenant, trace_id, span_id)`, so two tenants sharing
a trace id can never upsert over each other. A tenant-bound credential stamps
its binding onto spans that name no tenant, and a span claiming a *different*
tenant fails the **whole batch** with `400` — loudly and batch-atomically,
because silently rewriting it would hide a misconfigured exporter forever.

The key is `$tenant`, not `tenant`, and the `$` earns its place: a span's
top-level namespace is open, so a bare `tenant` is client data preserved
verbatim (it is not, and never becomes, an identity). Reserving `$tenant` as
`$payload` is reserved means a store written before tenancy reads back
correctly — its bare `tenant` values stay data — rather than a value being
promoted to an identity no query selects and no erasure names. The `?tenant=`
read filter and the erasure subject's `tenant` field are their own closed
namespaces and keep the plain name.

**Errors.**

| Response | Cause |
|---|---|
| `400 {"error":"body must be an array or {spans: [...]}"}` | Body's first non-whitespace byte is neither `[` nor `{` |
| `400 {"error":"missing field \`name\` at line 1 column 31"}` | A required field is absent; serde names it |
| `400 {"error":"span 0: trace_id is empty"}` | Empty primary-key half, at batch index 0 |
| `400 {"error":"span 0: span_id is empty"}` | As above |
| `400 {"error":"span 0: tenant does not match the credential's binding"}` | A bound credential's span claims another tenant |
| `400 {"error":"tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes"}` | An inadmissible tenant identity, refused by the engine |
| `503 {"error":"…"}` | The store rejected the write |

Validation is atomic per batch: one invalid span stores none of them.

### `POST /v1/traces`

OTLP/HTTP ingest. `Content-Type: application/x-protobuf` selects the binary
decoder; anything else is parsed as OTLP/HTTP JSON.

**Response `200` (JSON request).**

```json
{"partialSuccess":{}}
```

Spans a pending [erasure](#erasure) suppressed were acknowledged and not
stored, and the response says so in OTLP's own vocabulary:
`{"partialSuccess":{"rejectedSpans":N,"errorMessage":"…"}}`.

**Response `200` (protobuf request).** `Content-Type: application/x-protobuf`
with a zero-length body — the encoding of an empty `ExportTraceServiceResponse`.
With suppressions, the body encodes
`partial_success { rejected_spans, error_message }` instead, for the same
reason: an empty response is a claim of full success, and it would be false.

**Tenancy.** OTLP has no top-level tenant field, so Traza reads the
`traza.tenant` **resource attribute**: set it on the exporter's resource and
every span in the export lands under that tenant. A bound credential stamps
and checks it exactly as native ingest does, and an invalid `traza.tenant` is
a `400` for the **whole export**, deliberately loud — OTLP's `partialSuccess`
is for data the server chose to drop, not for a defect the client must fix.

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

### `POST /v1/checkpoint`

Publishes a generation: seals the buffer, writes a manifest naming every
load-bearing file with its digest, and moves `CURRENT`. The server also does
this every five minutes. See [backup and restore](../operations/backup.md).

```json
{"generation":42}
```

Errors: `503` if the checkpoint failed. Nothing is published unless it
succeeds — the prior generation stays live.

### `POST /v1/backups/{label}`

Checkpoints, hard-links the manifested files into `pins/{label}`, and verifies
every digest before reporting success. The server keeps running throughout;
the pin holds its bytes even after compaction unlinks the originals, so the
copy can proceed at its own pace.

```json
{"backup":"nightly","generation":42,
 "path":"/var/lib/traza/pins/nightly","verified":true}
```

`path` is a directory to copy — Traza never writes outside its own data
directory. Copy it, then release the pin.

Errors: `409` if a pin by that label already exists or the label is not a
single non-hidden path component; `500` with a `problems` array if the pin does
not verify; `503` if the checkpoint or linking failed.

### `POST /v1/backups/{label}/release`

Removes a pin, freeing the disk its unshared bytes were holding. Idempotent.

```json
{"released":true,"backup":"nightly"}
```

### `GET /v1/verify`

Re-reads and re-digests every file in the live generation's manifest.

```json
{"generation":42,"intact":true,"problems":[]}
```

`problems` names each discrepancy — `segment-….seg: digest mismatch`,
`payloads/ab/cd.bin: missing` — because which file is damaged is what decides
whether to restore. A read, so an `ro` token may ask.

---

## Query

### `GET /v1/spans`

Filtered span search, ordered by Traza's stable span order
(`start_time_ns`, `end_time_ns`, `tenant`, `trace_id`, `span_id`) — the
tenant joined the ordering key when it joined the primary key.

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
| `tenant` | string | Only spans of this tenant. **An empty value names the default tenant explicitly; omitting the parameter matches every tenant** (the operator view). A bound credential has its own tenant forced here. Also accepted by `/v1/export`, `/v1/tail`, and every `/v1/stats/*` route |
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
than a silently wrong page. The token format changed when the tenant joined
the ordering key — a leading version byte now identifies the layout — so a
pre-tenancy cursor parses as **invalid** (`400 {"error":"cursor is not a token
this server issued"}`), never as a plausible wrong position. Re-issue the
query without a cursor and page afresh.

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
attached to that trace. The id is percent-decoded from the path. Accepts
`?tenant=`: an empty value names the default tenant explicitly, omitting it
searches every tenant; a bound credential is forced onto its own, and naming
a foreign tenant is a `403`.

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

### `GET /v1/tail`

Streams spans as they are admitted, as [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html).

**This is the only route ordered by admission rather than event time**, and
that is the reason it exists. `GET /v1/spans?since=` answers "what *started*
after time T", which cannot express "what is arriving": a span that runs longer
than the client's polling interval starts before the watermark and arrives
after it, so a polling tail drops it permanently. Not late — never. The tail
assigns each admission a sequence number instead, so a span is delivered when
it lands regardless of when it started.

**Parameters.** Every predicate [`GET /v1/spans`](#get-v1spans) accepts, except
the event-time window, plus:

| Parameter | Type | Description |
|---|---|---|
| `cursor` | string | Resume position from a previous frame. Omit to start fresh |
| `backfill` | integer | Spans of history to open with. Default **200**, `0` for future-only. Ignored when `cursor` is given |
| `limit` | integer | Maximum spans per frame. Default **200** |

`since`, `since_ns`, `until` and `until_ns` are **rejected** with `400` rather
than ignored — a tail cannot honour an event-time window, and silently dropping
one would answer a different question than the one asked.

**Response `200`.**

```
Content-Type: text/event-stream
Transfer-Encoding: chunked
Connection: close
```

Three frame types:

```
event: spans
data: {"spans":[{...}],"cursor":"1763913600000000000.41"}

event: gap
data: {"missed":20}

: tick
```

A `spans` frame carries matches in admission order and the position to resume
from. Its `spans` array may be empty while `cursor` still advances — that means
spans were admitted but none matched the filter, and the position moves so a
narrow subscriber does not fall off the back of the ring.

A **`gap`** frame means the subscriber fell further behind than the server
retains. **It carries no position, deliberately.** The dropped entries are
precisely the ones no longer addressable, and `/v1/spans` is ordered by event
time, so it cannot name an admission range at all — there is no query that
fetches "what was dropped". `missed` is how many admissions were lost, or
`null` when the cursor came from a previous process and the count would not be
comparable.

A gap is a **discontinuity**. The server restarts the subscription at the live
edge and the next `spans` frame is a fresh backlog; the client discards what it
was holding and rebuilds from that frame. One ordered source, no overlap, and
no claim of completeness across the break. Spans lost to a gap are still in the
store — they are reachable through [`GET /v1/spans`](#get-v1spans), just not as
part of this stream.

A line beginning `:` is a comment — the heartbeat, sent every 15 seconds on a
quiet store. It is how an intermediary is kept from reaping the connection and
how the server discovers a client has gone.

**A gap does not change `backfill`.** After a gap the stream resumes with the
backlog you asked for — including none, if you asked for `backfill=0`. The
server does not substitute a default.

**A streamed span has been acknowledged.** Entries reach the stream only after
the ingest that carried them succeeded — after the write-ahead log's fsync, and
after the seal that `--durability flushed` promises. The tail is bounded and may
gap, but it never shows a span the store did not accept, and never one that a
crash a moment later would erase.

**Cursors do not survive a restart.** The `epoch` half of the token identifies
the process; a cursor from a previous one is reported as a gap rather than
misread as a live position.

**Retention is bounded and in memory.** The server keeps recent admissions
under two bounds, whichever binds first: `--tail-ring-spans` (default **8192**)
and `--tail-ring-bytes` (default **32 MiB**). Beyond either, a subscriber gaps.
The byte bound is the one that actually caps memory — the ring owns whole spans,
and an LLM span carrying a prompt is orders of magnitude larger than one
carrying a status code. This costs no disk and adds no field to the stored span.
Current residency and both bounds are reported at
[`GET /v1/metrics.json`](#get-v1metricsjson) under `tail_ring`.

This route always closes its connection, and is counted but never timed in
[route-class metrics](../operations/monitoring.md).

```sh
curl -N 'http://localhost:8080/v1/tail?service=support-agent&backfill=0'
```

---

## Sessions and analytics

### `GET /v1/sessions`

Sessions active in the window, most recent activity first.

**Session identity is `(tenant, session_id)`**: two tenants reusing an id are
two sessions, never one merged row — the primary key doing for sessions what
it does for spans.

| Parameter | Type | Description |
|---|---|---|
| `since` / `since_ns` | integer | Window start, Unix nanoseconds |
| `until` / `until_ns` | integer | Window end, Unix nanoseconds |
| `limit` | integer | Maximum sessions returned. Default **100** |
| `tenant` | string | Only this tenant's sessions. Empty names the default tenant explicitly; omitted lists every tenant's (the operator view, each row carrying its tenant) |

`group_by` is explicitly rejected here: `400 {"error":"group_by is not a
/v1/sessions parameter"}`.

**Response `200`.**

```json
{"sessions":[{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"llm_calls":1,"prompt_tokens":412,"session_attribute":"gen_ai.conversation.id","session_id":"chat-4711","span_count":1,"total_tokens":500,"trace_count":1}]}
```

| Field | Type | Description |
|---|---|---|
| `session_id` | string | The identifier shared by the session's spans |
| `tenant` | string | Whose spans formed the session; **omitted for the default tenant** |
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
and resolved under any recognized session key. Accepts `?tenant=` with the
same semantics as [`GET /v1/traces/{trace_id}`](#get-v1tracestrace_id) —
session identity is `(tenant, session_id)`, so the same id under two tenants
is two different sessions.

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
| `tenant` | string | Only this tenant's spans. Empty names the default tenant explicitly; omitted aggregates every tenant |

With `group_by=session`, rows merge **structurally** by `(tenant, session_id)`
and only then render a display key: a non-default tenant's row renders `key`
as `tenant/session_id`. Merging by the rendered string instead would fuse a
default-tenant session literally named `acme/chat` with tenant `acme`'s
session `chat` — two rows may still *render* alike, but merged counters would
be wrong where equal text is merely honest.

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

Records one annotation against a **typed subject**, without mutating any
span. The subject is expressed by which address fields are set, and exactly
one of four shapes must hold:

| Subject | Address fields | Meaning |
|---|---|---|
| **trace** | `trace_id` alone | Judgment about the trace as a whole |
| **span** | `trace_id` + `span_id` | Judgment about one span |
| **session** | `session_id` alone | Judgment about a session as a whole |
| **experiment example** | `experiment_id` + `example_id` | A **score**. Optionally also `trace_id`/`span_id`, naming the task run's span — which is what makes a score address the `(experiment, example, span)` tuple |

**Body.**

| Field | Type | Required | Description |
|---|---|---|---|
| `trace_id` | string | by subject | Trace containing the annotated span; on a score, the run's trace |
| `span_id` | string | no | Annotated span; requires its `trace_id` |
| `tenant` | string | no | The annotation's tenant; empty is the default tenant. Scoped exactly like span identity — reads filter on it, erasure dooms by it. `$tenant` is accepted as an alias so the span's spelling routes correctly. A bound credential stamps its binding onto an empty value and answers `400` when a different one is named |
| `session_id` | string | by subject | Session-subject address |
| `experiment_id` | integer | by subject | Experiment half of a score's address |
| `example_id` | string | with `experiment_id` | Example half of a score's address — the stable example id within the experiment's dataset version |
| `name` | string | yes | For example `quality`, `thumbs`, `groundedness` |
| `value` | any JSON | yes | Number, string, boolean, object |
| `source` | string | no | Convention: `human:<who>` or `eval:<evaluator>` |
| `comment` | string | no | Free-form |
| `timestamp_ns` | integer | no | Unix nanoseconds. **Absent or `0` is filled in with the server's current time** |

**A score's address must hold at write time**, validated against the eval
log under its own lock: the experiment must exist (`400
{"error":"no experiment 7"}`), belong to the score's tenant (`400
{"error":"a score's tenant must be its experiment's; experiment 7
disagrees"}`), and list the example in its dataset version's manifest (`400
{"error":"example ex-1 is not in experiment 7's dataset version"}`). Each
refusal names the offender, because a scorer that fires thousands of these
needs to know *which* one was misaddressed.

**Scores are exempt from TTL.** They live on eval retention, not trace
retention: a rolling window that swept January's scores would silently empty
the base of every experiment-over-experiment diff run in March.

```sh
curl -X POST http://localhost:8080/v1/annotations \
  -H 'Content-Type: application/json' \
  -d '{"trace_id":"trace-2","span_id":"span-a","name":"groundedness",
       "value":0.9,"source":"eval:nightly"}'
```

```json
{"recorded":true}
```

While an [erasure](#erasure) covering the addressed subject is pending — the
trace or span, the session, or the annotation's whole tenant — the annotation
is acknowledged and deliberately not stored: the same admission barrier spans
get, so no judgment can attach itself to data mid-erasure and surface after
the settle.

**Errors.** `400` with the parse error for a malformed body; `400` when the
engine rejects the annotation as invalid; `503` on store failure.

### `GET /v1/annotations`

Every supplied narrowing must match. **All of them are optional** — an empty
query returns every annotation, newest first.

| Parameter | Type | Description |
|---|---|---|
| `trace_id` | string | Narrow to one trace |
| `span_id` | string | Narrow to one span |
| `tenant` | string | Narrow to one tenant; empty names the default tenant. Bound credentials are forced onto their own |
| `session_id` | string | Narrow to session-subject annotations for this session id |
| `experiment_id` | integer | Narrow to scores of this experiment |
| `example_id` | string | Narrow to scores of this example |
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

Results are newest first, tie-broken by
`(tenant, trace_id, span_id, example_id, name)` so a bulk import sharing one
timestamp still pages deterministically.

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

## Evals

The eval entity model: datasets → versions (immutable, content-addressed) →
experiments → runs, with **scores riding the annotations surface** (a score
is an annotation addressed to an experiment example — see
[`POST /v1/annotations`](#post-v1annotations)). Identity and addressing only:
no runner, no scorer library, no UI. Task execution stays outside Traza by
design — what these entities close is that telemetry alone cannot *represent*
the eval loop: a failing production trace has nowhere to be promoted to, and
an experiment has no identity to hang scores on.

Everything lives in one append-only JSONL log (`evals.jsonl` at the store
root), fsynced per mutation, a recovery domain exactly like
`annotations.jsonl` and `tombstones.jsonl`.

**Tenancy applies throughout.** Datasets carry a tenant (empty is the default
tenant) and experiments and runs inherit it. A bound credential sees only its
own tenant's entities, and **cross-tenant reads answer `404`, never `403`** —
existence is a fact about another tenant, and a distinguishable refusal would
leak it.

### `POST /v1/datasets`

**Body.** `{"name": "...", "tenant": "..."}` — `tenant` optional; empty is
the default tenant, and a bound credential's binding is stamped or checked
exactly as at span ingest.

```json
{"dataset_id":1}
```

Dataset ids are store-wide, monotonic, and **never reused** — even a tenant
erasure that removes the highest-id datasets leaves a counter record behind,
so an erased id can never be silently re-issued to alias a different dataset
in external references (CI configs, receipts, operator notes).

**Errors.** `400 {"error":"dataset name is empty"}`; `400` for an invalid
tenant; `403` for a bound credential naming a foreign tenant; `409` while an
erasure of the tenant is pending.

### `GET /v1/datasets`

Datasets visible to the caller, each with version summaries. Accepts
`?tenant=` (empty names the default tenant; omitted lists every tenant's, for
operators).

```json
{"datasets":[{"schema":1,"dataset_id":1,"tenant":"acme","name":"support-hard-cases","created_unix_ns":1755200000000000000,"versions":[{"version_id":"63ff05…","examples":12,"created_unix_ns":1755200000000000000,"tombstoned":false}]}]}
```

`tenant` is omitted on default-tenant records; a version summary carries
`parent` only when the version has one.

### `GET /v1/datasets/{id}`

One dataset in the same shape, or `404 {"error":"no such dataset"}` — for an
unknown id and for another tenant's alike.

### `POST /v1/datasets/{id}/versions`

Creates an immutable, content-addressed dataset version.

**Body.**

| Field | Type | Required | Description |
|---|---|---|---|
| `parent` | string | no | The version this one derives from, for lineage. Must be a version of the same dataset, and not tombstoned |
| `provenance` | any JSON | no | What produced this version — a query, an import description. Recorded verbatim; on an idempotent re-POST the *first* write's provenance stands |
| `examples` | array | yes | At least one of `{example_id, input, expected?, split?, provenance?}` |

`example_id` is client-chosen and **stable across versions** — the id is
identity and the digest is content, so a version that re-lists an id with new
content is new lineage for the same logical example. `input` and `expected`
are free JSON and may contain `$payload` references copied from a promoted
span; those count as live references, which is what makes the copy real for
offloaded content. `provenance` on an example names the source
`{tenant?, trace_id?, span_id?}` it was promoted from.

**Response `200`.**

```json
{"version_id":"9c4f…","examples":12,"created":true}
```

`version_id` is the SHA-256 of the canonical manifest
`{dataset_id, parent, examples}`, so **identical content is the identical
version and a re-POST is idempotent by construction** — it answers the same
`version_id` with `"created":false`.

**Errors.**

| Response | Cause |
|---|---|
| `400 {"error":"a version needs at least one example"}` | Empty manifest |
| `400 {"error":"example_id is empty"}` | An example without identity |
| `400 {"error":"example ex-1 appears twice in the manifest"}` | Duplicate example id |
| `400 {"error":"no dataset 7"}` | Unknown dataset — or another tenant's |
| `400 {"error":"parent version 9c4f… is not a version of dataset 7"}` | Foreign or unknown parent |
| `409 {"error":"parent version 9c4f… is tombstoned"}` | Lineage cannot extend a deleted version |
| `409 {"error":"payload sha256/… is not in the store; the example would be born dangling"}` | A `$payload` reference whose bytes do not exist. An example must not be born dangling — "examples carry their own copies" would be a lie at birth |
| `409 {"error":"payload sha256/… is pending erasure; the promotion conflicts with it"}` | The reference is being erased right now |

### `GET /v1/datasets/{id}/versions/{vid}`

The version's manifest **and** its example bodies, in manifest order:

```json
{"schema":1,"tenant":"acme","dataset_id":1,"version_id":"9c4f…","examples":[["ex-1","63ff05…"]],"created_unix_ns":1755200000000000000,"bodies":[{"example_id":"ex-1","digest":"63ff05…","body":{"input":{"prompt":"why"},"expected":"because","split":"test"}}]}
```

Three answers, deliberately distinct: `200` with manifest and bodies; `410`
with the tombstone when the version was logically deleted — the content is
withheld, the fact of the deletion is not; `404
{"error":"no such dataset version"}` when it never existed (or belongs to
another tenant).

### `POST /v1/datasets/{id}/versions/{vid}/tombstone`

**Requires the `admin` scope**, like erasure: a version tombstone is a
deletion, and a credential minted to write telemetry must not be able to
perform it. **Body.** `{"reason": "..."}`, optional; the reason is recorded
verbatim.

```json
{"tombstoned":true,"already":false,"version_id":"9c4f…"}
```

Idempotent — tombstoning twice answers `"already":true`.

This is **logical** deletion with defined effects:

- fetching the version answers `410` carrying the tombstone;
- dependent experiments **keep working** — their runs and scores are their
  own — and report `dataset_version_deleted: true` so a reader knows the
  dataset content is no longer served;
- new experiments against the version are refused with `409`;
- scores are untouched;
- example bodies stay in the log, and their `$payload` references keep
  counting as live, until a future eval compaction reclaims
  version-unreachable bodies. To destroy promoted *content*, tombstone the
  version and erase the payload — see
  [Erasure](#erasure).

### `POST /v1/experiments`

**Body.** `{"dataset_id": 1, "dataset_version": "9c4f…", "name": "...",
"config": {...}}` — `name` and `config` optional; `config` is free JSON
(model, prompt hash, temperature, …) recorded verbatim.

```json
{"experiment_id":1}
```

Experiment ids are monotonic and never reused, like dataset ids. The tenant
is inherited from the dataset.

**Errors.** `400 {"error":"no dataset 7"}`;
`400 {"error":"version 9c4f… is not a version of dataset 7"}`;
`409 {"error":"version 9c4f… is tombstoned; new experiments cannot run
against it"}`; `409` while an erasure of the tenant is pending.

### `GET /v1/experiments`

Accepts `?dataset_id=` and `?tenant=`, both optional narrowings.

```json
{"experiments":[{"schema":1,"experiment_id":1,"tenant":"acme","dataset_id":1,"dataset_version":"9c4f…","name":"prompt-v2","created_unix_ns":1755200000000000000,"dataset_version_deleted":false,"run_count":12}]}
```

### `GET /v1/experiments/{id}`

One experiment in the same shape, or `404 {"error":"no such experiment"}`.
`dataset_version_deleted` reports whether the version it ran against has been
tombstoned since; `run_count` counts recorded task runs.

### `POST /v1/experiments/{id}/runs`

Records one task run: the experiment→trace link. The harness runs the task
and ingests the trace as ordinary telemetry; this records which example the
trace answers.

**Body.** `{"example_id": "ex-1", "trace_id": "trace-9", "span_id":
"span-3"}` — `span_id` optional, when the harness knows the run's span.

```json
{"recorded":true}
```

Append-only, duplicates legal: a retried example is two runs.

**Errors.** `400 {"error":"run trace_id is empty"}`;
`400 {"error":"no experiment 7"}`; `400 {"error":"example ex-1 is not in
experiment 7's dataset version"}`; `409` while an erasure of the tenant is
pending.

### `GET /v1/experiments/{id}/runs`

```json
{"runs":[{"schema":1,"tenant":"acme","experiment_id":1,"example_id":"ex-1","trace_id":"trace-9","span_id":"span-3","created_unix_ns":1755200000000000000}]}
```

`404` for an unknown (or foreign) experiment.

### `GET /v1/experiments/{id}/scores`

The experiment's score annotations, newest first — the same records
[`GET /v1/annotations?experiment_id=`](#get-v1annotations) returns. Accepts
`?limit=`.

```json
{"scores":[{"trace_id":"trace-9","span_id":"span-3","experiment_id":1,"example_id":"ex-1","name":"groundedness","value":0.9,"source":"eval:nightly","comment":"","timestamp_ns":1755200000000000000}]}
```

### `GET /v1/experiments/{id}/summary`

Per-name score distributions over one experiment.

```json
{"experiment_id":1,"examples_total":12,"scores":[{"name":"groundedness","count":12,"examples_scored":12,"mean":0.84,"min":0.2,"max":1.0,"p50":0.9,"p95":1.0},{"name":"pass","count":12,"examples_scored":12,"mean":0.75,"min":0.0,"max":1.0,"p50":1.0,"p95":1.0,"true_rate":0.75}]}
```

| Field | Description |
|---|---|
| `examples_total` | Examples in the experiment's dataset version manifest |
| `count` / `examples_scored` | Scores counted after dedup — one per scored example |
| `mean` / `min` / `max` / `p50` / `p95` | Over numeric values; booleans count as 0/1. Omitted when no value is numeric |
| `true_rate` | Fraction of boolean scores that were `true`; omitted when none were boolean |

**Scores deduplicate per `(example_id, name)` before any statistic**: the
highest `(timestamp_ns, trace_id, span_id)` wins — the store's
last-write-wins character applied to an append-only log, so a re-scored
example moves a number rather than double-counting, and history stays in the
log.

### `GET /v1/experiments/diff`

Experiment-over-experiment comparison: `?base=` and `?candidate=` name the
two experiment ids, and scores join on `(example_id, name)` after the same
dedup as the summary.

```json
{"base":1,"candidate":2,"scores":[{"name":"groundedness","mean_base":0.84,"mean_candidate":0.91,"delta":0.07,"improved":["ex-3","ex-7"],"regressed":["ex-1"],"unchanged":9,"only_base":[],"only_candidate":["ex-12"]}]}
```

Per name: `improved` and `regressed` list example ids whose value moved,
`unchanged` counts the rest scored in both, `only_base` / `only_candidate`
list examples scored on one side only. Numeric values compare
**higher-is-better, with booleans as 0/1** — a documented convention, not
configurable in this milestone; it is what makes "pass" diffable at all.
Numeric *strings* are deliberately not coerced: scores are machine-written,
and a writer that stringifies numbers should hear about it early.

**Errors.** `400 {"error":"diff needs base= and candidate= experiment ids"}`;
`404 {"error":"no such experiment"}` when either side is unknown or foreign.

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

For an unbound credential the full hash **is** the capability: knowing it
means having read a span that disclosed it. For a **tenant-bound** credential
that argument fails across the boundary — the hash of guessable content is
computable, and a `200` would confirm another tenant stored those exact bytes
— so a bound fetch must additionally prove **reachability**: some span or
dataset example of its own tenant carries the reference. An unreachable
payload answers `404` exactly as an absent one does. See
[administration § Payload fetch across tenants](../operations/administration.md#payload-fetch-across-tenants).

**Errors.** `404 {"error":"payload not found"}`. `503` on store failure.

---

## Tenants

### `GET /v1/tenants`

Per-tenant usage accounting: one exact fold over a pinned snapshot. A bound
credential gets exactly its own row; an unbound one gets every tenant's, the
default tenant included as `"tenant":""`.

```sh
curl http://localhost:8080/v1/tenants
```

```json
{"tenants":[{"tenant":"acme","spans":412,"traces":37,"bytes_approx":1048576,"payload_bytes_approx":300000,"first_start_ns":1700000000000000000,"last_end_ns":1700003600000000000}]}
```

| Field | Type | Description |
|---|---|---|
| `tenant` | string | The tenant; empty is the default tenant |
| `spans` | integer | Logical spans currently visible — LWW-resolved, superseded versions excluded. What the tenant sees, not what disk holds |
| `traces` | integer | Distinct traces among them |
| `bytes_approx` | integer | Approximate serialized size of those spans, inline content only |
| `payload_bytes_approx` | integer | Bytes of distinct offloaded payloads the tenant's spans reference, from the reference objects' recorded sizes. A blob shared across tenants counts for **every** referencing tenant — for a quota question, each of them is holding the store to those bytes |
| `first_start_ns` | integer | Earliest span start |
| `last_end_ns` | integer | Latest span end |

**This is an exact fold and it is O(store)** — it decodes the tenant's
visible spans to count them, on demand. It is an accounting answer, not a
metric to poll per minute against a hundred-million-span corpus; for
continuous monitoring use [`/v1/metrics`](#get-v1metrics), and reach for this
route when the question is "what does each tenant hold right now".

**Errors.** `503` on store failure.

---

## Erasure

Targeted deletion by **subject**, with a receipt to prove it. TTL removes
spans by age; erasure removes them because someone is entitled to have them
gone, and the difference is what must be demonstrable afterwards. The
operational story — what remains on purpose, pins, re-delivery — is in
[administration § Erasure](../operations/administration.md#erasure-deletion-with-a-receipt).

There is deliberately **no MCP tool for erasure**: the agent-facing surface
stays read-only, so stored adversarial text has no deletion verb to actuate.

### `POST /v1/erasures`

**Requires the `admin` scope** when `TRAZA_TOKENS` is configured — an `rw`
token gets `403 {"error":"forbidden"}`. Erasure is a POST like any ingest, so
the method rule cannot distinguish it, and it must be distinguished: a
credential minted to write telemetry must not be able to destroy it. See
[administration § Authentication](../operations/administration.md#authentication).

Erases one subject and blocks until the erasure settles. The `200` is the
acknowledgement, and it means the whole sequence ran, in an order chosen so
each artifact stays true: barrier → purge → confirm → checkpoint → settle.
The intent is fsynced into the tombstone log; from that moment covered spans
are dropped at admission **before payload offloading** (acknowledged, not
stored, no bytes written; counted in
`traza_erasure_spans_suppressed_total` and reported in the ingest response),
and covered annotations are dropped at [`POST /v1/annotations`](#post-v1annotations)
the same way. The purge and a confirm pass then rewrite the buffer,
write-ahead log, segments (superseded versions included) and annotation log,
and delete payload files reference-aware. Only after every rewrite does the
checkpoint publish — durable at the `CURRENT` rename — so the generation the
settle record cites digests exactly the store the erasure left behind, and
`GET /v1/verify` holds against it. The cut is exact: **every span
acknowledged before `settled_unix_ns` is erased or was never stored.**

```sh
curl -X POST http://localhost:8080/v1/erasures \
  -H 'Content-Type: application/json' \
  -d '{"subject": {"kind": "trace", "trace_id": "trace-1"}}'
```

| Subject | Fields | Erases |
|---|---|---|
| `trace` | `trace_id`, `tenant` | Every span of **one tenant's** trace, its annotations, payload bytes no survivor references |
| `span` | `trace_id`, `span_id`, `tenant` | One span by primary key, all physical versions |
| `session` | `session_id`, `tenant` | Every span of one tenant resolving to the session, across all recognized session keys |
| `payload` | `reference` | The `sha256/<hex>` file, plus a rewrite of every referencing span dropping its inline preview. Carries **no tenant** — content addressing is store-global — and is reserved for unbound admin credentials |
| `tenant` | `tenant` (non-empty) | Everything the tenant owns: its spans across every domain, its annotations and scores, its datasets, versions, examples and experiments, and reference-aware deletion of the payload bytes its spans and examples held |

On trace/span/session subjects `tenant` is optional and **empty means the
default tenant, never "all tenants"** — two tenants sharing a trace id are
two subjects, and erasing one leaves the other untouched, which is the
primary key doing its job. Every subject's `tenant` also accepts `$tenant`,
so the span's reserved spelling cannot silently aim a deletion at the default
tenant. The `tenant` subject requires a non-empty name:
the default tenant is every store that never configured tenancy, and "erase
it whole" is "erase the store", which is not an API — narrower subjects
express every legitimate deletion
(`400 {"error":"a tenant subject names a non-empty tenant"}`).

**Tenant subjects record no span keys, deliberately.** Every other subject is
resolved at request time to the concrete `(tenant, trace_id, span_id)` keys
it covers, bounded by the subject's own size; a tenant's key set is
unbounded, the mask and the purge cover by predicate instead, and for a whole
tenant **the settle time is the re-delivery line**: any of the tenant's data
ingested before `settled_unix_ns` is erased or was never stored, anything
after is new data. While a tenant erasure is pending, eval mutations for that
tenant are refused with
`409 {"error":"tenant acme has an erasure pending; retry after it settles"}`
rather than raced.

**A bound admin erases its own tenant and nothing else** — the
tenant-subject arm very much included, or a per-tenant admin could destroy a
neighbour wholesale. Naming a foreign tenant answers
`403 {"error":"this credential is bound to a different tenant"}`; a payload
subject answers `403 {"error":"payload subjects are store-global; erasing one
requires an unbound admin credential"}`. Bound credentials also see only
their own tenant's records at [`GET /v1/erasures`](#get-v1erasures), and a
foreign erasure id answers `404`.

**Response `200`.** The erasure record — id, subject (payload hashes are
canonicalized to lowercase before anything is resolved or recorded) — plus
its `settle` block: `spans_removed`, `spans_redacted`,
`annotations_removed`, `eval_records_removed` (datasets, versions, examples,
experiments, runs and version tombstones removed from the eval log — nonzero
only for tenant subjects; a tenant's *scores* are annotations and count under
`annotations_removed`), `payloads_removed`, `payloads_retained` (each
retained reference carries its reason — shared content still referenced by
live spans outside the subject is kept, and the receipt says so), and the
`generation` that published the deletion. The counts are the settling pass's
counts: after a crash-resume the first pass's work is already done and cannot
be re-counted, so the authority on absence is always the receipt.

Erase and settle records are written at **schema 2**: `span_keys` are
`(tenant, trace_id, span_id)` triples, and the trace/span/session subjects
carry their tenant. Schema-1 records — two-element `(trace_id, span_id)`
pairs — replay as the default tenant; arity decides the decoding, never a
guess.

Reads and ingest continue throughout. From the moment the tombstone is
recorded the subject is invisible to every query — before the rewrites run —
and a crash mid-erasure leaves a pending erasure that masks at the next open
and is finished by the server's maintenance tick. Spans ingested under the
same identifiers **after** the erasure settles are new data: a tombstone is a
barrier, not a ban.

**Errors.** `400` on a malformed body or subject. `503` on store failure.

### `GET /v1/erasures`

Every erasure the tombstone log records, oldest first, each with its `settle`
block or `null` while pending. `GET /v1/erasures/{id}` returns one.

### `GET /v1/erasures/{id}/verify`

The **erasure receipt**: re-checks every domain the subject's bytes could
inhabit and reports the result of each, by name — write buffer, live tail,
write-ahead log, segments, annotations, **eval records**, payload files,
derived caches, pins, generation metadata, and the tombstone log itself
(retained by design: the record of the erasure is what this receipt verifies
against).

The `eval-records` domain sits between annotations and payloads, and what it
reports depends on the subject:

- for **trace/span/session** subjects it lists examples in the subject's own
  tenant's datasets whose provenance or content is traceable to the subject —
  promoted **copies**, which survive source erasure by design. These report
  as `attention` (the receipt stays inconclusive): purging them is a
  deliberate second act — tombstone the version, erase the payload — not a
  side effect of erasing the source;
- for **payload** subjects it lists examples still carrying the reference.
  These report as `retained-by-design` and stay conclusive: after the blob's
  deletion the reference is a dangling **address, not content** — the bytes
  are gone and the version's digests remain valid;
- for **tenant** subjects it re-counts the records the tenant still owns,
  which must be zero.

The `payloads` domain accounts for the union of the references recorded at
resolve time and every disposition the settle recorded. The union matters
most for tenant subjects, whose resolve-time list is deliberately empty (the
purge collects their refs as it walks) — without the settle's lists the
domain would verify nothing and a shared payload's retention would go
unnamed.

```sh
curl http://localhost:8080/v1/erasures/1/verify
```

`result` is `erased` or `incomplete`, computed from what the walk found and
never from what the settle record claims. Matches are classified against the
erase record's resolved keys under one rule for every domain: an erased key
found live again is a **re-delivery** and fails the receipt; a fresh key
matching the subject is **new activity** and is reported without failing it.
A pin created before the erasure still holds the subject's bytes in its
hard-link farm, and the receipt says which pin to release.

The byte-level occurrence scans (the log, rollup sidecars) are
over-approximate on purpose — an identifier quoted in unrelated content
counts — so their findings report as `attention` and are carried in a
separate top-level field: **`conclusive`** is `false` whenever any
over-approximate signal was found and not proven benign. `result` answers
what the semantic walk found; `conclusive` answers whether anything at all
was left ambiguous. A receipt offered as proof should be `erased` **and**
`conclusive`.

The same receipt is available offline, against a directory no live server
owns:

```sh
traza-server verify --erasure 1 --data-dir /var/lib/traza
```

Exits `0` when the receipt is erased and conclusive, `3` when it is erased
but inconclusive, `2` when the erasure did not hold.

**Errors.** `404` for an id the tombstone log does not record. `400` on a
non-numeric id. `503` on store failure. The walk probes segment indexes
rather than decoding the corpus, but it is a real verification, not a cache
read — expect it to cost more than a query.

---

## Operations

### `GET /v1/stats`

```json
{"buffer_age_seconds":4,"buffered_records":1,"bytes_on_disk":0,"durability":"wal","persisted_records":0,"record_count":1,"segment_count":0,"total_records":1,"wal_bytes":261}
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
| `buffer_age_seconds` | integer or null | Seconds the oldest buffered span has waited for a seal; `null` when the buffer is empty. Climbing past `--max-buffer-age-seconds` means nothing is scheduling buffer maintenance |

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

### `POST /v1/mcp`

The [Model Context Protocol](../guide/mcp.md) endpoint: JSON-RPC 2.0 over the
Streamable HTTP transport, one message per request. **Served only when the
server was started with `--mcp`**; otherwise every method answers `404` with
the flag named.

This route is documented in full in **[the MCP guide](mcp.md)** — its ten
tools, five resources, three templates and four prompts, and what each returns.
What belongs here is the transport contract:

| Condition | Response |
|---|---|
| A JSON-RPC request | `200` with one JSON-RPC response |
| A JSON-RPC notification or response | `202` with **no body** |
| `GET` or `DELETE` | `405` — no server-initiated SSE stream, no session to delete |
| `Origin` present, and neither loopback nor named by `--mcp-allowed-origin` | `403` — the transport's DNS-rebinding defence. The origin is never validated against the request's own `Host`, which a rebinding request also controls |
| `MCP-Protocol-Version` naming an unserved revision | `400`, listing the supported ones |
| A body that is not JSON | `400` with JSON-RPC `-32700` |
| A JSON array (a batch) | `400` with JSON-RPC `-32600` — batching was removed from MCP |

Supported protocol revisions are `2025-11-25` and `2025-06-18`. A revision this
server serves is echoed back from `initialize` unchanged; anything else is
answered with `2025-11-25` and the client decides whether to continue.

**Authorization is per tool, not per method.** Unlike every other route here,
the HTTP method does not describe the operation: MCP tunnels reads and writes
alike through one `POST`. A `ro` token therefore reaches every read tool, and
`record_annotation` additionally requires `rw` *and* `--mcp-annotations`.

```sh
curl -X POST http://localhost:8080/v1/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

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
