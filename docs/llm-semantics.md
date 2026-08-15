# LLM observability semantics

Conventions for tracing generative-AI workloads with Traza. They are plain
span attributes and events — no server or engine support beyond recognizing
the keys, and every recipe below runs against the standard filter API today.

Prerequisites: the [data model](guide/data-model.md) for what a span is, and
the [HTTP API reference](guide/http-api.md) for the routes these recipes use.

Traza follows the [OpenLLMetry](https://github.com/traceloop/openllmetry)
standard — Traceloop's OpenTelemetry-based conventions for LLM and agent
tracing (the `opentelemetry-semantic-conventions-ai` package), built on the
OpenTelemetry GenAI semantic conventions. Point any OpenLLMetry- or OTel-GenAI-
instrumented app at Traza over OTLP and its sessions, token counts, cost, and
provider/model rollups populate with **no attribute renaming**. Traza's earlier
native `llm.*` / `session.id` shorthand is still accepted as an alias, so
existing instrumentation keeps working unchanged.

The single source of truth for the key precedence is
[`src/semconv.rs`](../src/semconv.rs); the dashboard mirrors it in
`ui/src/lib/spans.js`.

## Recognized attributes

Each fact is resolved from the first key present, in this order. Current OTel
GenAI names come first; the names OTel has since deprecated are kept as
aliases, and Traza's native `llm.*` shorthand last:

| Fact | Keys (first present wins) |
|---|---|
| Provider | `gen_ai.provider.name` → `gen_ai.system` *(deprecated)* |
| Model | `gen_ai.response.model` → `gen_ai.request.model` → `llm.model` |
| Prompt tokens | `gen_ai.usage.input_tokens` → `gen_ai.usage.prompt_tokens` *(deprecated)* → `llm.prompt_tokens` |
| Completion tokens | `gen_ai.usage.output_tokens` → `gen_ai.usage.completion_tokens` *(deprecated)* → `llm.completion_tokens` |
| Total tokens | `llm.usage.total_tokens` → `gen_ai.usage.total_tokens` → `llm.total_tokens` → (prompt + completion) |
| Session id | `session.id` → `gen_ai.conversation.id` → `traceloop.association.properties.session_id` → `traceloop.association.properties.chat_id` |

A span counts as an **LLM call** when it carries any of: a model, a provider,
a token count, `gen_ai.operation.name`, `llm.request.type`, or
`traceloop.span.kind == "llm"`. Numeric token values may be supplied as numbers
or numeric strings (some OTLP exporters stringify counters); an explicit total
wins over the prompt+completion sum.

**Cost is a Traza extension, not an OpenLLMetry attribute.** OpenTelemetry
GenAI defines no cost attribute — providers meter cost out of band. Traza
reads cost from `llm.cost_usd` (and accepts `gen_ai.usage.cost` as a courtesy
for pipelines that compute it); it is not part of OpenLLMetry conformance.

Most instrumentation does not meter cost, which used to mean a store that knew
the model and both token counts still reported `$0.00` everywhere. Give the
server a [pricing table](configuration.md#model-pricing) and it derives a cost
for those calls instead:

```sh
traza-server --data-dir ./data --port 8080 --pricing ./pricing.json
```

A metered `llm.cost_usd` always wins — the table only fills blanks, and only
when the span reported both an input and an output token count. Derived cost
is summed separately from metered cost and reported as `cost_derived_usd`
beside `cost_usd` on `/v1/stats/llm` and `/v1/sessions`, so a total's
provenance is always answerable; the dashboard prefixes any figure containing
an estimate with `~`.

Attributes are indexed like any other, so exact-match filters on them are
index-served.

**A filtering gotcha worth knowing before you write a query.** `attr.KEY=VALUE`
parses `VALUE` as JSON when it is valid JSON and treats it as a plain string
otherwise. Token counts illustrate the trap, because the analytics path accepts
them as numbers *or* numeric strings but the filter path compares by exact JSON
equality:

    GET /v1/spans?attr.gen_ai.usage.input_tokens=412       # matches the NUMBER 412
    GET /v1/spans?attr.gen_ai.usage.input_tokens=%22412%22 # matches the STRING "412"

So a rollup can count a stringified token value while a bare numeric filter
misses the same span. Model and provider names are unaffected — they are not
valid JSON, so they fall back to strings as you would expect. Full rules:
[filter value types](guide/data-model.md#filter-value-types).

## Span names and kinds

OpenLLMetry names spans by operation and provider — `openai.chat`,
`anthropic.chat`, `{workflow}.workflow`, `{task}.task`, `{tool}.tool` — not by
a fixed `llm.completion` name. To select LLM spans portably, filter on the
Traceloop span kind rather than the name:

    GET /v1/spans?attr.traceloop.span.kind=llm

Traceloop's workflow model is carried by `traceloop.span.kind`
(`workflow`/`task`/`agent`/`tool`/`llm`), `traceloop.workflow.name`, and
`traceloop.entity.name` / `traceloop.entity.path`. Traza stores and indexes
these verbatim, so they are all filterable.

## Sessions and aggregation

An agent session usually spans many traces. Any span carrying a recognized
session key (above) joins that session. `GET /v1/sessions` lists sessions with
span and distinct-trace counts, token sums, cost, error counts, and the
`session_attribute` that grouped each one; `GET /v1/sessions/{id}` adds the
per-trace breakdown and resolves the id under any recognized key.

To list a session's spans, use the dedicated session filter — it **unions**
every recognized key, so a session that spans conventions (some spans
`session.id`, others `gen_ai.conversation.id`) returns whole, which a
single-key `attr.session.id` filter cannot do:

    GET /v1/spans?session=chat-4711&limit=100

`GET /v1/stats/llm?group_by=model|provider|service|session|day` aggregates
token/cost figures. `provider` groups by the resolved provider
(`gen_ai.provider.name`, else `gen_ai.system`); `model` by the resolved model.
Aggregates are exact: sealed segments contribute cached rollups (segments are
immutable, so a rollup is computed once), and window edges fall back to
decoding just the boundary segments.

## Prompt and completion payloads

Chat content rides three shapes, all recognized by the dashboard's Messages
panel:

- **Current OTel GenAI messages** (OpenLLMetry default) — JSON-encoded
  `gen_ai.input.messages` and `gen_ai.output.messages`, each an array of
  `{"role": …, "parts": [{"type": "text", "content": …}, …]}` objects
  (tool-call parts are rendered compactly);
- **Legacy indexed attributes** — `gen_ai.prompt.{i}.role` /
  `gen_ai.prompt.{i}.content` and `gen_ai.completion.{i}.role` /
  `gen_ai.completion.{i}.content`;
- **Native events** — event `llm.prompt` with attribute `content`, event
  `llm.completion` with attribute `content`.

All round-trip verbatim through `/v1/spans` and OTLP ingest. Large content is
offloaded at ingest (below), so it never bloats the attribute index.

### Media parts

Within a messages array, the dashboard renders media parts across the
spellings emitters actually produce — images inline, audio and video with
players, documents and object-store locators as downloadable references:

- **OTel GenAI / Traza native** — `{"type": "image"|"audio"|"video"|
  "document"|"file", "mime_type": …, "data": <data: URI or bare base64>,
  "uri"|"url": …, "filename": …, "size_bytes": …, "width"/"height": …}`;
- **OpenAI** — `{"type": "image_url", "image_url": {"url": …}}`,
  `{"type": "input_audio", "input_audio": {"data": …, "format": "wav"|"mp3"}}`,
  and `{"type": "file", "file": {"filename": …, "file_data"|"file_id": …}}`;
- **Anthropic** — `{"type": "image"|"document", "source": {"type":
  "base64"|"url"|"file", "media_type": …, "data"|"url"|"file_id": …}}`;
- **Google GenAI** — typeless parts carrying `inline_data`/`inlineData`
  (`{"mime_type": …, "data": …}`) or `file_data`/`fileData`
  (`{"mime_type": …, "file_uri": …}`);
- **Tool results** — an MCP-style `{"content": [ …parts… ]}` list inside a
  `tool_call_response`/`tool_result` part renders its parts, screenshots
  included.

Bare base64 bytes are lifted into `data:` URIs using the declared MIME type;
`data:`/`http(s):` sources render in place; `s3://`, `gs://` and other
non-fetchable locators stay references with a copy affordance. A part whose
bytes the emitter declined to capture (`capture_status`/`archive_status`
`"unavailable"`) says so, with the emitter's `unavailable_reason`. A whole
messages attribute past the offload threshold arrives as a payload
reference; the conversation view fetches small ones back automatically (and
larger ones on demand) and renders the parsed turns, so offloading never
demotes media to a JSON dump.

## Query recipes

All spans for one model (index-served attribute filter):

    GET /v1/spans?attr.gen_ai.request.model=gpt-4o&limit=100

Chat spans for one service:

    GET /v1/spans?service=agent&attr.traceloop.span.kind=llm

Slowest LLM calls (duration filter over LLM-kind spans):

    GET /v1/spans?attr.traceloop.span.kind=llm&min_duration_ms=2000

Tool-call frequency for one tool (Traceloop tool spans):

    GET /v1/spans?attr.traceloop.span.kind=tool&attr.traceloop.entity.name=web_search

`GET /v1/stats/llm` provides server-side cost and token totals grouped by
model, provider, service, session, or UTC day. Integer counters saturate at
`u64::MAX` rather than wrapping, and non-finite cost strings are ignored.

Remember that `/v1/spans` applies a **default limit of 100**; pass `limit`
explicitly when a recipe should return more. `/v1/export` is unbounded by
default and is the right route for anything dataset-sized.

## Payloads and annotations

Prompt/completion text longer than the server's payload threshold is
offloaded at ingest: the attribute (or event attribute) keeps a
`{"$payload": "sha256/…", "bytes": N, "preview": "…"}` reference and
`GET /v1/payloads/{ref}` serves the bytes. Content addressing stores a
repeated system prompt once. Post-hoc judgment — eval scores, human
feedback — attaches via `POST /v1/annotations` without mutating spans, and
`GET /v1/export` turns any span filter into a chunked NDJSON dataset. Export
clients must verify the terminal `X-Traza-Export-Complete: true` trailer and
may cross-check `X-Traza-Export-Count`.

## Span links

Agentic traces are not trees: parallel tool calls fan out and rejoin,
retries reference earlier attempts, and one agent's span may cause work in
another agent's trace. Spans carry a `links` array (`trace_id`, `span_id`,
`attributes`) on both ingest surfaces; OTLP links map 1:1 with hex ids
lowercased. A conventional `relation` link attribute (for example
`retry-of`, `spawned`, `joins`) keeps link semantics queryable.

## OTLP mapping

The same attributes flow through `POST /v1/traces`: OTLP `intValue`
attributes become numbers, `doubleValue` stays a double, string values stay
strings, and events map to span events — so OpenLLMetry / OTel GenAI
instrumentation lands queryable without translation. `service.name` on the
OTLP resource becomes the span's service. gRPC is not served; use the
`http/protobuf` exporter setting every OTel SDK supports.

The complete OTLP field mapping is in [ingest](guide/ingest.md#otlp-mapping).

## See also

- [Getting started](guide/getting-started.md) — point an OTel SDK at Traza
- [Data model](guide/data-model.md) — spans, primary key, attribute types
- [HTTP API reference](guide/http-api.md) — every route and parameter
- [Trace browser](guide/trace-browser.md) — the conversation and analytics views
- [`src/semconv.rs`](../src/semconv.rs) — the single source of truth for key
  precedence, mirrored by `ui/src/lib/spans.js` and pinned by
  `tests/llm_semantics.rs` and `tests/openllmetry_conformance.rs`
