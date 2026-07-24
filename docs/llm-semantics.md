# LLM observability semantics

Conventions for tracing generative-AI workloads with Traza. They are plain
span attributes and events — no server or engine support beyond recognizing
the keys, and every recipe below runs against the standard filter API today.

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

Each fact is resolved from the first key present, in this order:

| Fact | Keys (first present wins) |
|---|---|
| Provider | `gen_ai.system` |
| Model | `gen_ai.response.model` → `gen_ai.request.model` → `llm.model` |
| Prompt tokens | `gen_ai.usage.prompt_tokens` → `gen_ai.usage.input_tokens` → `llm.prompt_tokens` |
| Completion tokens | `gen_ai.usage.completion_tokens` → `gen_ai.usage.output_tokens` → `llm.completion_tokens` |
| Total tokens | `llm.usage.total_tokens` → `gen_ai.usage.total_tokens` → `llm.total_tokens` → (prompt + completion) |
| Cost (USD) | `gen_ai.usage.cost` → `llm.cost_usd` |
| Session id | `session.id` → `gen_ai.conversation.id` → `traceloop.association.properties.session_id` → `traceloop.association.properties.chat_id` |

A span counts as an **LLM call** when it carries any of: a model, a provider
(`gen_ai.system`), a token count, `llm.request.type`, or
`traceloop.span.kind == "llm"`. Numeric token/cost values may be supplied as
numbers or numeric strings (some OTLP exporters stringify counters); an
explicit total wins over the prompt+completion sum.

Attributes are indexed like any other, so exact-match filters on them are
index-served.

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

`GET /v1/stats/llm?group_by=model|provider|service|session|day` aggregates
token/cost figures. `provider` groups by `gen_ai.system`; `model` by the
resolved model. Aggregates are exact: sealed segments contribute cached
rollups (segments are immutable, so a rollup is computed once), and window
edges fall back to decoding just the boundary segments.

## Prompt and completion payloads

Chat content rides two shapes, both recognized:

- **OpenLLMetry indexed attributes** — `gen_ai.prompt.{i}.role` /
  `gen_ai.prompt.{i}.content` and `gen_ai.completion.{i}.role` /
  `gen_ai.completion.{i}.content`;
- **Native events** — event `llm.prompt` with attribute `content`, event
  `llm.completion` with attribute `content`.

Both round-trip verbatim through `/v1/spans` and OTLP ingest, and the
dashboard renders them as a Messages panel. Large content is offloaded at
ingest (below), so it never bloats the attribute index.

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
