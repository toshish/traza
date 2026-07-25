# Data model

Traza stores one kind of record: a span. Everything else — traces, sessions,
token rollups, waterfalls — is derived from spans at read time.

## The span

```jsonc
{
  "trace_id": "trace-1",          // required, non-empty
  "span_id": "span-1",            // required, non-empty
  "parent_span_id": null,         // optional; null or absent for a root span
  "name": "charge",               // required
  "service": "checkout",          // required
  "start_time_ns": 1700000000000000000,  // required, Unix nanoseconds
  "end_time_ns":   1700000000002500000,  // required, Unix nanoseconds
  "status": "ok",                 // optional, defaults to ""
  "attributes": {},               // optional, any JSON values
  "events": [],                   // optional
  "links": []                     // optional
}
```

`trace_id`, `span_id`, `name`, `service`, `start_time_ns` and `end_time_ns`
have no defaults — omitting one is a `400` naming the missing field. The rest
default as shown.

### Timestamps

Integer Unix nanoseconds, both fields. On ingest, `start_time_ns` also accepts
the aliases `start_time_unix_nano`, `start_timestamp_ns`, `start_ns`, and
`start_time`; `end_time_ns` accepts the matching `end_*` forms. Responses
always use the canonical `start_time_ns` / `end_time_ns`.

A span's duration is `end_time_ns - start_time_ns`, and that is what
`min_duration_ms` / `min_duration_ns` filter on. Traza does not validate that
`end` follows `start`.

### Attributes

Arbitrary JSON: strings, numbers, booleans, null, and nested objects and
arrays. Attribute values are indexed for exact-match filtering, so
`attr.KEY=VALUE` searches are index-served rather than scans.

Attribute keys carrying a leading NUL byte are stored verbatim but never
indexed — the index reserves that prefix for span fields, and the filter path
declines the index for such keys symmetrically. In practice no real key hits
this.

### Events and links

**Events** are timestamped records inside a span:

```json
{"name": "llm.prompt", "timestamp_ns": 1700000000001000000, "attributes": {"content": "…"}}
```

**Links** point at other spans, possibly in other traces:

```json
{"trace_id": "trace-9", "span_id": "span-3", "attributes": {"relation": "retry-of"}}
```

Links exist because agentic traces are not trees: parallel tool calls fan out
and rejoin, retries reference earlier attempts, and one agent's span may cause
work in another agent's trace. A `relation` attribute by convention keeps the
semantics queryable. `links` is omitted from responses when empty.

### Unknown fields survive

Any field Traza does not recognize is stored and returned verbatim at the top
level of the span. Ingesting

```json
[{"trace_id": "t10", "span_id": "s10", "name": "n", "service": "svc",
  "start_time_ns": 1, "end_time_ns": 2, "my_custom": {"a": 1}}]
```

reads back with `"my_custom":{"a":1}` intact. This is a wire contract, not an
accident: a client may carry its own fields through Traza without them being
silently dropped. Unknown top-level fields are *not* indexed — put anything you
want to filter on in `attributes`.

## `(trace_id, span_id)` is the primary key

This is the single most important thing to understand about ingesting into
Traza.

A span is uniquely named by the pair `(trace_id, span_id)`. Ingesting a span
whose pair already exists **replaces** the stored version. The newest ingest
wins — last-write-wins — and no second copy is created.

```sh
curl -X POST http://localhost:8080/v1/spans -H 'Content-Type: application/json' \
  -d '[{"trace_id":"lww","span_id":"s1","name":"first","service":"svc",
        "start_time_ns":10,"end_time_ns":20,"status":"error"}]'

curl -X POST http://localhost:8080/v1/spans -H 'Content-Type: application/json' \
  -d '[{"trace_id":"lww","span_id":"s1","name":"second","service":"svc",
        "start_time_ns":10,"end_time_ns":20,"status":"ok"}]'

curl http://localhost:8080/v1/traces/lww
```

returns exactly one span, the second one:

```json
{"annotations":[],"spans":[{"attributes":{},"end_time_ns":20,"events":[],"name":"second","parent_span_id":null,"service":"svc","span_id":"s1","start_time_ns":10,"status":"ok","trace_id":"lww"}],"trace_id":"lww"}
```

### What this means in practice

**Client retries are idempotent.** If a batch times out, or a `503` tells you
to back off, or your exporter is not sure whether the request landed — resend
it. Re-sending a span that already arrived replaces it with an identical copy.
You cannot create duplicates by retrying, so retry logic needs no
deduplication, no idempotency key, and no client-side bookkeeping.

**Spans can be updated.** Send a span when it starts with the fields you have,
then send it again when it completes with the full picture. The completed
version replaces the partial one. This is how you correct a span whose status
or token counts were not known at first emission.

**Ids must be genuinely unique.** Because the pair is a key, two logically
different spans that share it collide into one. Both halves must be non-empty,
and both ingest surfaces reject an empty id with a `400` naming the offending
index (`{"error":"span 0: span_id is empty"}`) rather than accepting a batch
whose spans would silently merge.

**Superseded versions stay on disk until compaction.** Last-write-wins is
resolved at read time, so a replaced span is hidden from every query
immediately but its bytes remain until a compaction rewrites the segment. This
is why `/v1/stats` reports *physical record* counts rather than logical span
counts — see [monitoring](../operations/monitoring.md#get-v1stats).

## Traces

A trace is not a stored object. `GET /v1/traces/{trace_id}` collects every span
carrying that `trace_id`, ordered by start time, and returns it with any
annotations attached to the trace. There is no separate "create a trace" step
and no requirement that a root span exist or arrive first. Spans may arrive in
any order, from any number of processes.

## Sessions

A session groups traces into one unit of agent work — a conversation that spans
many requests. A span joins a session by carrying a recognized session key;
Traza resolves the first present of `session.id`, `gen_ai.conversation.id`,
`traceloop.association.properties.session_id`, then
`traceloop.association.properties.chat_id`.

Because that is a *union* of keys, the dedicated `session=` filter is not the
same as `attr.session.id=`: the former returns a session whole even when its
spans use mixed conventions, the latter sees one key. Full detail in
[LLM semantics](../llm-semantics.md).

## Annotations

Spans are immutable once ingested, but judgment about them arrives later — an
eval score, a human thumbs-down, a triage label. Annotations are a separate
record type attached to a `(trace_id, span_id)` (or to a whole trace when
`span_id` is empty) without mutating the span. They are returned alongside a
trace by `GET /v1/traces/{trace_id}` and queried directly at
`GET /v1/annotations`. See the [API reference](http-api.md#post-v1annotations).

## Payload references

String attribute values above the configured threshold (256 KiB by default) are
moved out of the span at ingest and replaced by a reference object:

```json
{"$payload": "sha256/29927e…", "bytes": 300000, "preview": "first characters…"}
```

The bytes are fetched from `GET /v1/payloads/sha256/{hex}`. Storage is
content-addressed, so a system prompt repeated across ten thousand calls is
stored once. This keeps multi-megabyte prompts out of every decode that touches
the span. See
[payload offloading](../operations/administration.md#payload-offloading).

## Filter value types

Search filters compare attribute values by exact JSON equality, which makes the
*type* of the value you send significant. `attr.KEY=VALUE` parses `VALUE` as
JSON when it is valid JSON, and falls back to a plain string when it is not:

| Query | Matches |
|---|---|
| `attr.region=us-east` | the string `"us-east"` (not valid JSON, so a string) |
| `attr.code=200` | the **number** `200` |
| `attr.code=%22200%22` | the **string** `"200"` |
| `attr.flag=true` | the **boolean** `true` |
| `attr.flag=%22true%22` | the **string** `"true"` |
| `attr.thing=null` | JSON `null` |

The consequence worth remembering: to match a *string* attribute whose value
looks like a JSON literal — `"200"`, `"true"`, `"null"` — you must send it
quoted and URL-encoded (`%22200%22`). A bare `200` will not match the string
`"200"`.
