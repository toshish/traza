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

A span may also carry a top-level `$tenant` — lowercase
`[a-z0-9][a-z0-9._-]`, at most 64 bytes. Omitted or empty means the DEFAULT
tenant, and an empty tenant is never serialized back, so a store that never
uses tenants writes files with no tenant bytes in them at all. A credential
bound to a tenant (see [administration](../operations/administration.md))
stamps it for you and refuses a contradiction.

The `$` sigil is load-bearing, not decoration. A span's top-level namespace is
open — any unknown field survives in the round trip (see below) — so a bare
`tenant` key is *your* data and stays your data, exactly as it was before the
identity existed. Reserving `$tenant`, the way payload references reserve
`$payload`, is what lets a store written before tenancy be read after it
without a value ever being mistaken for an identity. A bare `tenant` you send
is preserved verbatim and is never an identity; put a value you mean as
identity under `$tenant`, or let a bound credential supply it.

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

`timestamp_ns` accepts the aliases `time_unix_nano` — OTLP's spelling, and the
one most clients build events with — plus `timestamp_unix_nano`, `time_ns`,
and `time`. `attributes` may be omitted for an event that is just a named
instant; it reads back as `{}`. Only `name` and a timestamp are required.
Responses always use the canonical `timestamp_ns`.

**Links** point at other spans, possibly in other traces:

```json
{"trace_id": "trace-9", "span_id": "span-3", "attributes": {"relation": "retry-of"}}
```

Links exist because agentic traces are not trees: parallel tool calls fan out
and rejoin, retries reference earlier attempts, and one agent's span may cause
work in another agent's trace. A `relation` attribute by convention keeps the
semantics legible. `links` is omitted from responses when empty.

**Links are stored and returned, not indexed.** No search filter reaches a
link's attributes — `attr.`, `not_attr.` and content search all read the span's
own attributes — so a `relation` value is not something you can query for.
What reads links today is [`diagnose_session`](mcp.md), which traverses them
within the run it is already analyzing. An earlier version of this page said
link semantics were "queryable"; they were not, and are not.

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

## `(tenant, trace_id, span_id)` is the primary key

This is the single most important thing to understand about ingesting into
Traza.

A span is uniquely named by the triple `(tenant, trace_id, span_id)` — for a
single-tenant store, where every tenant is the default, that is exactly the
familiar `(trace_id, span_id)` pair. Ingesting a span whose key already
exists **replaces** the stored version. The newest ingest wins —
last-write-wins — and no second copy is created. Two tenants using the same
trace id hold two different keys, so they can never upsert over each other:
that is what the tenant is IN the key for.

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

A session is identified by `(tenant, session_id)`: the same `session.id`
value under two tenants is two sessions, never one merged row.

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
record type addressed to a TYPED SUBJECT, expressed by which address fields
are set (exactly one shape must hold):

- **a trace** — `trace_id` alone;
- **a span** — `trace_id` + `span_id`;
- **a session** — `session_id` alone, judging the conversation as a whole;
- **an experiment example** — `experiment_id` + `example_id`, optionally
  with `trace_id`/`span_id` naming the task run's span. This shape is a
  **score**: it addresses the `(experiment, example, span)` tuple the eval
  model needs (see below).

Every annotation is tenant-scoped like a span, but its identity key is plain
`tenant`, not `$tenant`: an annotation is a closed record with no open
namespace to protect, so there is nothing for the sigil to disambiguate.
`$tenant` is accepted here too, so a client that learned the span's spelling
routes correctly rather than silently landing in the default tenant. The same
holds for the erasure subject's `tenant` and a dataset's `tenant`. Annotations
are returned alongside a trace by `GET /v1/traces/{trace_id}` and queried
directly at `GET /v1/annotations`. See the
[API reference](http-api.md#post-v1annotations).

## Eval entities

Five entities make the eval loop representable — addressing only, no
workflow; task execution stays outside Traza:

- **Dataset** — a stable numeric id, a name, a tenant.
- **DatasetVersion** — an immutable, content-addressed manifest of
  `(example_id, digest)` pairs. Its id is the SHA-256 of the manifest, so
  identical content IS the identical version and re-promoting is
  idempotent. It records its `parent` version for lineage and the
  `provenance` of the promotion that produced it.
- **Example** — a stable, client-chosen id that persists across versions,
  with `input`, optional `expected`, a `split` label, and provenance back
  to the source span. **Examples carry their own copies**: large values
  arrive as `$payload` references, and those references count as live for
  retention and erasure — deleting the source trace cannot corrupt the
  version.
- **Experiment** — a stable numeric id binding one dataset version to a
  set of task runs, with free-form config metadata. Runs are recorded by
  the external harness (`POST /v1/experiments/{id}/runs`).
- **Score** — an annotation with the experiment-example subject, above.
  Distributions and experiment-over-experiment diffs are served by
  `/summary` and `/diff`, with per-`(example, name)` last-write-wins dedup
  so a retried scorer moves a number instead of double-counting it.

Deleting a dataset version is itself a tombstone with defined effects, and
erasure never silently destroys curated copies — the receipt names them.
See [the API reference](http-api.md) and
[administration](../operations/administration.md).

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

Search filters compare **scalars by value, not by type.** `attr.code=200`
matches a span that stored the number `200` and a span that stored the string
`"200"`:

| Query | Matches |
|---|---|
| `attr.region=us-east` | the string `"us-east"` |
| `attr.code=200` | the number `200` **and** the string `"200"` |
| `attr.flag=true` | the boolean `true` **and** the string `"true"` |
| `attr.thing=null` | JSON `null` |
| `attr.code=20` | neither — equality is not a prefix match |

This is deliberate, and it is a change from earlier versions. Instrumentation
is inconsistent about whether a status code or a token count is a number or a
string — several SDKs stringify both — and the old behaviour matched only the
JSON reading. A store full of stringified codes answered `attr.code=200` with
an empty array, which is indistinguishable from "no span has that code". A
filter that silently cannot match is worse than one that is merely strict.

Containers are exempt: arrays and objects still compare structurally, because
two different arrays that happen to render alike are not the same array.

Numeric comparisons use the same rule. `min_attr.llm.usage.total_tokens=1000`
reads the attribute as a number whether it was stored as `1000` or `"1000"`,
so a corpus with mixed conventions filters correctly.
