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
existing instrumentation keeps working unchanged. The
[OpenInference](https://github.com/Arize-ai/openinference) keys emitted by
Arize Phoenix instrumentation (`llm.model_name`, `llm.token_count.*`,
`openinference.span.kind`, …) are recognized too, ranked after every other
spelling so they only ever fill blanks.

The single source of truth for the key precedence is
[`src/semconv.rs`](../src/semconv.rs). The dashboard's trace, conversation
and compare views compute per-span usage, messages and step kind in the
browser from the same keys — OTel GenAI, OpenLLMetry, OpenInference and
native — mirrored in `ui/src/lib/spans.js`. The run, guard and diff views read
the server's own step facts (the tape, the verdicts, the alignment) and
compute none of them.

## Recognized attributes

Each fact is resolved from the first key present, in this order. Current OTel
GenAI names come first; the names OTel has since deprecated are kept as
aliases, then Traza's native `llm.*` shorthand, and the OpenInference spelling
last:

| Fact | Keys (first present wins) |
|---|---|
| Provider | `gen_ai.provider.name` → `gen_ai.system` *(deprecated)* → `llm.provider` → `llm.system` |
| Model | `gen_ai.response.model` → `gen_ai.request.model` → `llm.model` → `llm.model_name` |
| Prompt tokens | `gen_ai.usage.input_tokens` → `gen_ai.usage.prompt_tokens` *(deprecated)* → `llm.prompt_tokens` → `llm.token_count.prompt` |
| Completion tokens | `gen_ai.usage.output_tokens` → `gen_ai.usage.completion_tokens` *(deprecated)* → `llm.completion_tokens` → `llm.token_count.completion` |
| Total tokens | `llm.usage.total_tokens` → `gen_ai.usage.total_tokens` → `llm.total_tokens` → `llm.token_count.total` → (prompt + completion) |
| Session id | `session.id` → `gen_ai.conversation.id` → `traceloop.association.properties.session_id` → `traceloop.association.properties.chat_id` |

A span counts as an **LLM call** when it carries any of: a model, a provider,
a token count, `gen_ai.operation.name`, `llm.request.type`,
`traceloop.span.kind == "llm"`, or `openinference.span.kind == "LLM"` (any
letter case). Numeric token values may be supplied as numbers or numeric
strings (some OTLP exporters stringify counters); an explicit total wins over
the prompt+completion sum.

OpenInference's `llm.provider` names the host (`openai`, `azure`, `aws`) and
`llm.system` the model family (`openai`, `anthropic`); the host ranks first
because that is what `gen_ai.provider.name` means.

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
when the span reported both an input and an output token count.

Every cost-bearing row (`/v1/stats/llm`, `/v1/sessions`, and each bucket of
`/v1/stats/series`) reports `cost_derived_usd` beside `cost_usd` and three
call counts — `cost_metered_calls`, `cost_derived_calls`,
`cost_unpriced_calls` — so a total's provenance is always answerable.
**Read the counts, not the dollars:** a zero-rate model and a call nothing
could price both contribute `0`, so the money alone cannot tell them apart, or
tell either from a genuine measurement. See
[Model pricing](configuration.md#model-pricing).

### Prompt caching, and what "prompt tokens" means

`gen_ai.usage.cache_read.input_tokens` and
`gen_ai.usage.cache_creation.input_tokens` are recognized — with the older
underscore spellings (`cache_read_input_tokens`,
`cache_creation_input_tokens`) accepted as aliases, so a fleet running mixed
exporter versions is read correctly either way. They matter because with
caching on, providers disagree about what the prompt count *counts*:

| Provider | `input_tokens` / `prompt_tokens` | Context is |
|---|---|---|
| Anthropic, Bedrock, Claude on Vertex | the **uncached remainder** | prompt + cache read + cache creation |
| OpenAI, Azure OpenAI, Gemini on Vertex | **inclusive** of its cached subset | the prompt count itself |

Vertex (`gcp.vertex_ai`) serves both families under one provider name, so
there the model decides: a name containing `claude` is read as Anthropic's, one
containing `gemini` as inclusive, and any other Vertex model with cache
counters falls in the "not known" case below.

This matters wherever a growing conversation is being watched: read the
uncached remainder alone and a context that is growing appears to *shrink* as
its cache warms. [`diagnose_session`](guide/mcp.md) reads the resolved figure,
and where cache counters appear under a provider whose convention Traza does
not know, it reports the trend as unreadable rather than guessing. The
OpenInference host names (`aws`, `azure`, `google`) are not in the table
above, so they fall in that last case.

The same table decides how a derived cost counts cached tokens when the
pricing file names `cache_read_per_mtok` / `cache_write_per_mtok` for the
model: on top of the prompt for the exclusive providers, carved out of it for
the inclusive ones, and not at all for a provider not in the table (see
[Model pricing](configuration.md#model-pricing)).

OpenInference's own cache counters
(`llm.token_count.prompt_details.cache_read` / `.cache_write`) are **not**
read: OpenInference reports its prompt count inclusive of them, and folding
them into the cache fields would double-count the cached part.

### Session outcome and goal — Traza extensions

| Fact | Keys (first present wins) |
|---|---|
| Session outcome | `session.outcome` → `traceloop.association.properties.outcome` |
| Session goal | `session.goal` → `traceloop.association.properties.goal` |

Recognized values for outcome are `success`/`succeeded`/`ok`/`resolved`,
`failure`/`failed`/`error`, and `abandoned`/`cancelled`/`timeout`; anything
else is kept verbatim and reported as undetermined.

**Nothing in OpenTelemetry GenAI defines these** — the conventions describe
individual model calls, not the agent run around them — so, like cost, they
are Traza extensions and no existing pipeline emits them. That is why nothing
requires them: a session's outcome is **derived** from its own spans when
undeclared (the last step to finish decides, and a run still in flight is
reported as unknown rather than as a success). Declaring one replaces the
derivation with your own statement; it does not unlock the feature.

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

## Steps and runs

Traza also reads each span as one **step** of a **run**: what kind of work it
was, what it called, what went in and what came out, and a fingerprint that
says whether the same call with the same input happened before. The full
precedence is documented on `step_facts` and `run_id` in
[`src/semconv.rs`](../src/semconv.rs); this is the summary.

### Step kind

The first source carrying a recognized value decides; an unrecognized value
falls through to the next source. Values compare case-insensitively.

| Source | Values |
|---|---|
| `gen_ai.operation.name` | `chat`, `text_completion`, `generate_content` → **llm**; `execute_tool` → **tool**; `invoke_agent`, `create_agent` → **agent**; `embeddings` → **embedding** |
| `openinference.span.kind` | `LLM` → llm; `TOOL` → tool; `AGENT` → agent; `RETRIEVER` → **retrieval**; `EMBEDDING` → embedding; `CHAIN` → **other** |
| `traceloop.span.kind` | `llm` → llm; `tool` → tool; `agent` → agent; `workflow`, `task` → other |
| the span is an LLM call (above) | llm |
| `gen_ai.tool.name` or `tool.name` present | tool |
| none of these | other |

Only **llm**, **tool** and **agent** spans count as steps; retrievals,
embeddings and wrappers are context around them. Because any
`gen_ai.operation.name` makes a span an LLM call, an operation name not in the
first row lands at llm unless an OpenInference or Traceloop kind decided
first.

### Tool, agent and call id

| Fact | Keys (first present wins) |
|---|---|
| Tool | `gen_ai.tool.name` → `tool.name` → `traceloop.entity.name` *(only when `traceloop.span.kind` is `tool`)* |
| Agent | `gen_ai.agent.name` → `traceloop.entity.name` *(only when `traceloop.span.kind` is `agent` or `workflow`)* → `agent.name` |
| Tool call id | `gen_ai.tool.call.id` |

### Input and output

| Step kind | Input (first present wins) | Output (first present wins) |
|---|---|---|
| llm | `gen_ai.input.messages` → `gen_ai.prompt.{i}.*` → `llm.input_messages.{i}.message.*` → `input.value` → `llm.prompt` attribute → `llm.prompt` events | `gen_ai.output.messages` → `gen_ai.completion.{i}.*` → `llm.output_messages.{i}.message.*` → `output.value` → `llm.completion` attribute → `llm.completion` events |
| tool | `gen_ai.tool.call.arguments` → `traceloop.entity.input` → `input.value` | `gen_ai.tool.call.result` → `traceloop.entity.output` → `output.value` |
| any other | `traceloop.entity.input` → `input.value` | `traceloop.entity.output` → `output.value` |

Values are taken as stored: an offloaded value stays its `$payload` reference.
A string holding a JSON object or array is read as that JSON; any other string
stays text. The indexed layouts are reassembled into a message list in numeric
index order, each message carrying whichever of `role`, `content`,
`finish_reason`, `tool_call_id` and `tool_calls` (`[{id, name, arguments}]`)
was recorded; the OpenLLMetry and OpenInference layouts fold to that same
shape. Native events become `[{role, content}]` in event order. On an llm step
`gen_ai.system_instructions` becomes a leading `{"role": "system", …}` message
when the input is a message list (or the whole input when there is none).
When the input is not a list — an offloaded `$payload` reference, or a bare
prompt (text or a JSON object) — the input becomes
`[{"role": "system", …}, <input as stored>]`, so neither is dropped and the
digest covers both; a reader that resolves the reference combines the two as
if neither had been offloaded. A bare prompt combined this way is still not a
message list, which is why the replay proxy's strict mode refuses it.

Each input and output has a **digest**: the SHA-256 of its canonical JSON
(keys sorted, no whitespace), so the same messages sent with different key
order or spacing digest identically. An input that is wholly a `$payload`
reference digests to the content address it names, without reading the
payload — and since that address hashes the original attribute *text*, an
offloaded copy and an inline copy of the same input digest differently.

A step's **fingerprint** is `kind:name` — the tool name for a tool step, the
model for an llm step, the agent name for an agent step, otherwise (or when
that is absent) the span name — followed by `#` and the first 16 hex
characters of the input digest when there is an input; for example
`tool:web_search#50951da9bae64397`. Two steps with the same fingerprint made
the same call with the same input.

### Conversations recorded as deltas

| Attribute | Meaning |
|---|---|
| `gen_ai.conversation.id` | The conversation (thread) a model call belongs to. The [context X-Ray and Cache Autopsy](guide/xray.md#conversations-recorded-as-deltas) pair a call only with calls of its own conversation — the nearest earlier one, whatever they share. |
| `traza.input.delta` | Traza extension, a boolean (or exactly `"true"`). `true`: the call's input holds only the messages sent since its conversation's previous call, and its whole context is that call's context, then that call's output, then this input. Absent or `false`: the input is the whole context (the default, and the first call after a compaction). |

The [recorders](guide/recorders.md) set both on every model call they
convert, since a transcript records each message once. Nothing else reads
`traza.input.delta`: a tape, a replay and a diff show a call's input as
recorded.

### Run identity

A span's **run** is its session id (the session precedence above), else
`trace:<trace_id>`, within its tenant. A conversation spanning many traces is
therefore one run, and a trace with no session key is a run of its own.
Membership is per span, as it is for sessions: a trace whose root carries
`session.id` but whose children do not puts those children in the trace's run,
not the session's. A session id that itself begins with `trace:` reads as the
trace form.

### Handoffs

A run's **handoff sequence** is its steps that name an agent (the agent row
of the names table above), in step order — start time, then identity — with
consecutive steps of one agent merged into one **turn**. A step that names
no agent does not move control. A **handoff cycle** is control returning to
an agent straight from the agent it handed off to: turns `A, B, A`, counted
per ordered pair. The run guard's `max_handoff_cycles` limit and the run
topology both read this definition; see [steer](guide/steer.md#topology).

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
`retry-of`, `spawned`, `joins`) keeps link semantics legible, and
`diagnose_session` reads `retry-of` as corroboration when it is present.

Links are **stored and returned, not indexed**: no search filter reaches a
link's attributes, so a `relation` value cannot be queried for. It is read
inside a diagnosis of a run, not across the store.

## OTLP mapping

The same attributes flow through `POST /v1/traces`: OTLP `intValue`
attributes become numbers, `doubleValue` stays a double, string values stay
strings, and events map to span events — so OpenLLMetry / OTel GenAI
instrumentation lands queryable without translation. `service.name` on the
OTLP resource becomes the span's service. `service.version`,
`deployment.environment(.name)` and `telemetry.sdk.name`/`version` on the
resource are copied onto each span as attributes, which is where the
[change radar](guide/radar.md) reads deploys from. gRPC is not served; use the
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
