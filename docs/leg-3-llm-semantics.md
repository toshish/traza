# Leg 3: LLM-observability semantics

## Scope — ADDITIVE ONLY

A documented attribute schema plus tests. The ONLY files this leg may
touch: `docs/llm-semantics.md` (new), `tests/llm_semantics.rs` (new),
`README.md` (one section added). Touching any other file is out of scope
and grounds for gate rejection.

## Deliverables

1. `docs/llm-semantics.md`: the gen-AI span conventions —
   - span name convention (`llm.<operation>`: `llm.completion`,
     `llm.tool_call`, `llm.embedding`);
   - attributes: `llm.model` (string), `llm.prompt_tokens` /
     `llm.completion_tokens` / `llm.total_tokens` (ints),
     `llm.temperature` (double), `llm.tool_name` (string, tool calls),
     `llm.stop_reason` (string), `llm.cost_usd` (double, optional);
   - payload conventions: prompt and completion stored as span events
     named `llm.prompt` / `llm.completion` with a `content` attribute, so
     large payloads ride events rather than filterable attributes;
   - query recipes: cost-by-model, token totals by service, slowest
     completions, tool-call frequency — each as a concrete `/v1/spans`
     query the current API serves.
2. `tests/llm_semantics.rs`: process-level tests ingesting spans following
   the conventions (via both `/v1/spans` and OTLP `/v1/traces`) and
   asserting each documented query recipe returns the expected spans via
   the existing filter API (e.g. `attr.llm.model=...` uses the index).
3. README: a short "LLM observability" section linking the doc.

## Acceptance (blocking)

1. `./ci.sh` green; every existing test unmodified and passing.
2. The new test target passes and exercises every documented recipe.
3. `git diff --name-only` against the seed shows ONLY the three allowed
   paths (the additive-only oracle).

## Non-goals

Engine or server changes of any kind; new endpoints; SDKs.
