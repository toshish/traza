# LLM observability semantics

Conventions for tracing generative-AI workloads with Traza. They are plain
span attributes and events — no server or engine support is required, and
every recipe below runs against the standard filter API today.

## Span names

One span per model interaction, named by operation:

| Name | Use |
|---|---|
| `llm.completion` | A chat/completion call |
| `llm.tool_call` | The model invoking a tool/function |
| `llm.embedding` | An embedding request |

## Attributes

| Attribute | Type | Meaning |
|---|---|---|
| `llm.model` | string | Model identifier (for example `gpt-5.6-sol`) |
| `llm.prompt_tokens` | int | Tokens in the prompt |
| `llm.completion_tokens` | int | Tokens generated |
| `llm.total_tokens` | int | Prompt + completion |
| `llm.temperature` | double | Sampling temperature |
| `llm.stop_reason` | string | `stop`, `length`, `tool_use`, … |
| `llm.tool_name` | string | Tool invoked (`llm.tool_call` spans) |
| `llm.cost_usd` | double | Metered cost, if known (optional) |

Attributes are indexed like any other, so exact-match filters on them are
index-served.

## Prompt and completion payloads

Large text payloads ride EVENTS, not attributes, so they never enter the
attribute index and cannot bloat filter postings:

- event `llm.prompt`, attribute `content`: the prompt text;
- event `llm.completion`, attribute `content`: the generated text.

Events round-trip verbatim through both `/v1/spans` and OTLP ingest.

## Query recipes

All spans for one model (index-served):

    GET /v1/spans?attr.llm.model=gpt-5.6-sol&limit=100

Completions for one service (service + name are both indexed):

    GET /v1/spans?service=agent-web&name=llm.completion

Slowest completions (duration filter over completion spans):

    GET /v1/spans?name=llm.completion&min_duration_ms=2000

Tool-call frequency for one tool:

    GET /v1/spans?name=llm.tool_call&attr.llm.tool_name=web_search

Cost and token totals are client-side sums over these result sets (the API
returns the spans; aggregation endpoints are out of scope today).

## OTLP mapping

The same attributes flow through `POST /v1/traces`: OTLP `intValue`
attributes become numbers, `doubleValue` stays a double, and events map to
span events — so OpenTelemetry gen-AI instrumentation lands queryable
without translation.
