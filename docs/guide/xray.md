# Context X-Ray and Cache Autopsy

Traza already stores every request an agent sent to a model. This guide covers
reading one of those requests as what it was — **the context window the model
saw** — and asking it the questions people actually have when an agent goes
wrong or gets expensive:

- **What exactly did the model see?** The context split into chunks — system
  prompt, user turns, the model's own earlier words, tool calls, tool results,
  tool definitions — each with an estimated token count.
- **Where did each part come from?** The step of the run that introduced it:
  this tool result came from the `read_file` call at step 3; this paragraph is
  what the model itself said at step 6.
- **What is dead weight?** Tool results that have sat in the context for many
  steps without the model ever drawing on them, and content that is in the
  context twice.
- **What was lost?** What the conversation's previous call had that this one
  does not — the instruction a compaction summarized away.
- **Why did the prompt cache break, and what did it cost?** The **Cache
  Autopsy** compares every call with the one before it in the same
  conversation, names the first chunk that changed and the bytes that changed
  in it — usually one volatile token, a timestamp or a request id — and prices
  the tokens the cache could have served and did not.

All of it is deterministic arithmetic over stored spans, in the one
`traza-server` process. Traza never calls or embeds a model to do it, nothing
is learned, and nothing leaves the process. Route shapes are in the
[HTTP API reference](http-api.md#context-x-ray-and-cache-autopsy).

## See it in one call

The examples use a weather agent instrumented for Anthropic: a call that asks
for `get_weather`, the tool, and a call that answers. Its system prompt embeds
the current time — a common pattern, and a cache killer.

```sh
curl -s localhost:8080/v1/xray/4bf92f3577b34da6/m2
```

An excerpt of the real response, shares rounded (the full body is in the
[reference](http-api.md#get-v1xraytrace_idspan_id)):

```json
{
  "run": "weather-9",
  "step_index": 2,
  "model": "claude-sonnet-x",
  "chunks": [
    {"index": 0, "role": "system", "kind": "system", "est_tokens": 14,
     "introduced_by": {"step_index": 2, "kind": "llm", "name": "claude-sonnet-x", "via": "content"},
     "age_steps": 0, "stale": false,
     "preview": "You are a weather assistant. Now: 2026-10-14T09:00:02Z."},
    {"index": 1, "role": "user", "kind": "user", "est_tokens": 9,
     "introduced_by": {"step_index": 0, "kind": "llm", "name": "claude-sonnet-x", "via": "content"},
     "age_steps": 2, "preview": "Weather in Paris? Answer in Celsius."},
    {"index": 2, "role": "assistant", "kind": "tool_call", "name": "get_weather", "tool_call_id": "tu_1",
     "introduced_by": {"step_index": 0, "kind": "llm", "via": "content"},
     "preview": "get_weather({\"city\":\"Paris\"})"},
    {"index": 3, "role": "user", "kind": "tool_result", "tool_call_id": "tu_1",
     "introduced_by": {"step_index": 1, "kind": "tool", "name": "get_weather", "via": "content"},
     "age_steps": 1, "preview": "{\"temp_c\": 18, \"sky\": \"rain\"}"}
  ],
  "totals": {"chunks": 4, "est_tokens": 39, "reported_input_tokens": 96,
             "reported_context_tokens": 96, "cache_read": 0},
  "composition": {"by_source": [
    {"key": "system", "est_tokens": 14, "share": 0.359},
    {"key": "user", "est_tokens": 9, "share": 0.231},
    {"key": "tool calls", "est_tokens": 8, "share": 0.205},
    {"key": "tool:get_weather", "est_tokens": 8, "share": 0.205}]},
  "previous": {"step_index": 0, "shared_chunks": 2},
  "dropped_since_previous": [],
  "edited_since_previous": [
    {"index": 0, "previous_index": 0, "role": "system", "volatile": "timestamp",
     "diff": {"offset": 34, "before": "2026-10-14T09:00:01Z", "after": "2026-10-14T09:00:02Z"}}],
  "cache": {
    "cause": "volatile_content", "previous_step_index": 0, "prefix_chunks_shared": 0,
    "first_divergent_chunk": {"index": 0, "role": "system", "change": "edited", "volatile": "timestamp",
      "diff": {"offset": 34, "before": "2026-10-14T09:00:01Z", "after": "2026-10-14T09:00:02Z"}},
    "est_tokens_lost": 22, "est_cost_lost_usd": 0.0000594,
    "explanation": "chunk 0 (system) changed in place since step 0: \"2026-10-14T09:00:01Z\" → \"2026-10-14T09:00:02Z\" (a timestamp); ~22 tokens of content step 0 already had were re-processed"}
}
```

Read top to bottom: the user's question and the model's tool call came in with
the first call; the tool result came from the tool step; the system prompt is
new in this call — because one second of clock in it changed, which is also
why the provider's cache served nothing.

## The context window as chunks

The request a step recorded (its [input](../llm-semantics.md#input-and-output),
with `gen_ai.system_instructions` folded in as the first message) is split into
**one chunk per content part**. The message shapes of the conventions Traza
reads all fold to the same unit:

| Shape | Chunks |
|---|---|
| `{"role", "content": "text"}` (OpenAI, native, indexed OpenLLMetry and OpenInference layouts) | one chunk, kind from the role |
| `"tool_calls": [...]` on a message (OpenAI, indexed layouts) | one `tool_call` chunk per call |
| Anthropic content parts: `text`, `tool_use`, `tool_result`, `thinking`, `image`/`document` | one chunk per part |
| OTel GenAI `parts`: `text`, `tool_call`, `tool_call_response`, `blob`/`uri`/`file` | one chunk per part |
| Gemini `parts`: `text`, `functionCall`, `functionResponse` | one chunk per part |
| OpenAI Responses items: `message` (`input_text`/`output_text` parts), `function_call`, `function_call_output` | one chunk per part or item |
| A provider request body (`{"messages": [...], "system": ..., "tools": [...]}`, OpenInference's `input.value`) | its system, then its messages; its tools as definitions |
| A bare prompt (text, `llm.prompt`) | one `other` chunk with no role |

**Kinds.** `system` (roles `system`, `developer`), `user` (`user`, `human`),
`assistant` (the model's text, reasoning included), `tool_call`, `tool_result`
(role `tool`, Anthropic `tool_result` parts, OTel `tool_call_response` parts),
and `other` (tool definitions, media, unrecognized parts).

**Tool definitions come first**, under the pseudo-role `tools`, one chunk per
tool — from `gen_ai.tool.definitions`, OpenLLMetry's
`llm.request.functions.{i}.*`, OpenInference's `llm.tools.{i}.tool.json_schema`,
or a request body's `tools`. They are part of what the model is billed for and
part of the prompt a provider caches, which is why they lead: Anthropic orders
its cache prefix tools, system, messages.

A tool call's text is `name(arguments)`, the arguments canonical JSON. A tool
result's text is its content: a string, a list of text parts joined by
newlines, or a content envelope's text. A media part is shown as a placeholder
(`[image: image/png, 182044 bytes recorded]`) and estimates to zero tokens:
its encoding's length says nothing about what a provider bills for it.

**Token estimates** are about four ASCII characters, or one and a half other
characters, per token — an estimate, not a tokenizer, and labelled `est_` on
every field. The provider's own figures sit beside them in `totals`:
`reported_input_tokens` as reported, and `reported_context_tokens`, the
context resolved across providers' cache conventions (the table in
[prompt caching](../llm-semantics.md#prompt-caching-and-what-prompt-tokens-means)).

**Two identities per chunk.** `digest` (16 hex characters) identifies the
chunk's *content*: a JSON object or array held as text is canonicalized first,
and role and position are not part of it — so the same tool output matches
whether the tool span stored it parsed and the next prompt carried it as text,
or the other way round. The Cache Autopsy compares a second, internal identity
instead — role, kind, tool-call id and the text *exactly as recorded* —
because a provider's cache matches bytes: the same content re-serialized with
its keys in another order is new to the cache even though it is not new
content.

**Offloaded prompts are resolved.** A request offloaded to the payload store
(anything over `--payload-threshold-bytes`, 256 KiB by default — long agent
contexts usually are) is read back before it is chunked, under the caller's
tenant, and so are the run's earlier requests the analysis compares against.
`resolve=false` skips that: an offloaded request is then one opaque chunk,
`context_unavailable` says why, and the cache verdict is `unknown`.

## Provenance: where each chunk came from

`introduced_by` is **the earliest step of the run whose request or response
contained a chunk with the same content** — `step_index`, `kind`, its tool,
model or agent `name`, and its span. So:

- a tool result is introduced by the **tool step** that returned it;
- the model's own earlier text and tool calls, by the **model call that
  produced them** (its response);
- a user turn or a system prompt, by the **first call that carried it**;
- a sub-agent's task or answer, by the **agent step** that recorded it.

`via` is `content` for all of these. When an agent shows the model a tool
result different from what the tool returned — truncated, summarized,
annotated — no step holds the same content, and the chunk is attributed to the
tool step whose `gen_ai.tool.call.id` matches the result's call id, with `via:
"tool_call_id"`. That is worth noticing on its own: the model did not see what
the tool said.

`age_steps` is the steps between the introducing step and this call.
`composition.by_source` totals the context by where it came from —
`system`, `user`, `assistant`, `tool calls`, `tool definitions`, and
`tool:<name>` for each tool's results — so "61% of this context is `read_file`
output" is one field away.

## Stale tool results

A chunk is **stale** when it is a tool result older than
[`--xray-stale-steps`](../configuration.md#server-flags) steps (default 20;
`stale_steps=` per request) that **no model output since has drawn on**.

"Drawn on" is a heuristic, and is labelled one: some *distinctive word* of the
result — a run of four to sixty-four letters, digits or `_ . / -`, like
`parse_config` or `src/main.rs` — **first** appears in a model output (an
answer, or a tool call's arguments) after the result arrived. A word the model
had already said before the result arrived does not count; that is what keeps
common words from making everything look referenced. So a stale result is one
the model never echoed anything new from.

That is evidence, not proof, in both directions: a model can use a result
without quoting it, and can quote a word that happened to arrive from
elsewhere. But a long-lived result the model never echoed is the first thing to
cut when a context is too big, and `totals.stale_est_tokens` says how much
cutting it would save. When no model output after the result was recorded at
all, nothing is called stale — there is nothing to judge by.

## Duplicates

`duplicates` lists content present more than once — the same file read twice,
the same document retrieved into two turns — with the positions and the
estimated tokens of one copy; each repeat's chunk carries `duplicate_of`, and
`totals.duplicate_est_tokens` counts every copy after the first.

## What the previous call had: compaction loss

The X-Ray finds the conversation's **previous call** — the same predecessor
the Cache Autopsy uses (below) — and compares the two contexts:

- `dropped_since_previous`: chunks it had that this call does not. A dropped
  `system` or `user` chunk is flagged `"kind": "instruction"`: **a constraint
  the agent was given and may no longer see.** "Never edit files under
  migrations/" summarized away by a compaction shows up here, with the step
  that first introduced it.
- `edited_since_previous`: chunks that came back changed — the same role and
  kind, differing in a stretch at most a tenth of the text — with the first
  differing bytes and what kind of token they were. A re-stamped system prompt
  is an edit, not a lost instruction.

## Cache Autopsy

```sh
curl -s localhost:8080/v1/cache/weather-9
```

returns one verdict per model call of the run, in step order, and a summary
(full body in the [reference](http-api.md#get-v1cacherun)):

```json
{"summary": {"steps": 2, "breaks": 1,
             "by_cause": {"new_context": 1, "reordered": 0, "stable": 0,
                          "ttl_expired": 0, "unknown": 0, "volatile_content": 1},
             "est_tokens_lost": 22, "est_cost_lost_usd": 0.0000594, "disagreements": 0,
             "culprits": [{"cause": "volatile_content", "change": "edited", "role": "system",
                           "volatile": "timestamp", "breaks": 1, "est_tokens_lost": 22,
                           "example_step_index": 2,
                           "example": {"offset": 34, "before": "2026-10-14T09:00:01Z",
                                       "after": "2026-10-14T09:00:02Z"},
                           "advice": "A clock value sits inside the cached prefix. Move the time to the end of the prompt (after the last cache breakpoint), or round it to a coarser unit such as the day."}]}}
```

`culprits` groups the breaks by shape, costliest first — the kind of headline
that reads *a timestamp in the system prompt broke the cache on 47 of 48
calls* — and each carries `advice`: fixed guidance chosen by the shape (a
lookup table in the code, not generated text), such as moving a clock value
after the cached prefix, appending retrieved documents after the history
rather than ahead of it, or serializing tool output with sorted keys.

### The predecessor

A provider's prompt cache serves the longest previously-seen **prefix** of a
prompt, byte for byte, while the entry lives. So each call is compared with the
call before it **in the same conversation** — its predecessor: the nearest
earlier model call of the same tenant whose context shares at least half of its
chunks with this one, counted either way round (so a compaction that kept the
system prompt and replaced the history still finds the call it compacted). The
first chunks of the two contexts also count as shared when they are
near-identical, so a system prompt with a clock in it does not hide the
conversation it belongs to. Calls of *other* agents interleaved in the same run
share little and are skipped, which keeps a multi-agent run's autopsy per
conversation. The predecessor is searched among the previous 32 model calls.

A call with no predecessor is `new_context`: the first call of a conversation,
with nothing to cache yet. If the nearest earlier call of the tenant could not
be read (not recorded, or offloaded and unresolvable), the verdict is
`unknown` rather than a pairing across the gap.

### Causes

The two contexts are compared chunk by chunk from the start, on exact
identity; `prefix_chunks_shared` is how far they agree.

| `cause` | When | `first_divergent_chunk.change` |
|---|---|---|
| `stable` | The predecessor's whole context is an unchanged prefix of this one, within the TTL | — |
| `ttl_expired` | The prefix is intact, but the TTL had passed since the predecessor began | — |
| `volatile_content` | A chunk inside the prefix changed in place, the rest surviving | `edited` |
| | A new chunk sits ahead of content the predecessor already had | `inserted` |
| `reordered` | The same content in different bytes (key order, spacing), or under another role | `reserialized` |
| | Both differing chunks exist in both contexts, in other places | `moved` |
| `new_context` | No predecessor | — |
| | A chunk was removed from inside the prefix (pruning) | `removed` |
| | Less than half of what followed the divergence survives (compaction, truncation) | `rewritten` |
| `unknown` | This context is a strict prefix of the predecessor's: whether a shorter prompt hits depends on where the provider's cache breakpoints sat, which no trace records | `shrunk` |
| | This call's or the predecessor's request cannot be read | — |

The TTL defaults to 300 seconds — Anthropic's default cache lifetime and the
low end of OpenAI's — and `ttl_seconds=` sets it per request (up to a day) for
deployments on longer-lived caches. `gap_ns` is the time since the predecessor
*began*, which is when its prompt entered the cache. A structural divergence
outranks an expired TTL as the cause; the explanation mentions both.

### The volatile token

`first_divergent_chunk.diff` names the first differing stretch of the two
chunks' text: `offset` (a byte offset into the chunk's text), `before` and
`after`, widened to the whole word the difference falls in and clipped to 80
characters a side. For an in-place edit, `volatile` classifies what was
swapped: `timestamp` (a date, a time of day, or a 10/13/16/19-digit epoch),
`uuid`, `hex_id` (eight or more hex digits mixing digits and letters — request
ids, hashes), `number`, or `text`.

### Lost tokens and money

`est_tokens_lost` is the estimated tokens of content the predecessor already
had that this call carried **after** the break — what the cache could have
served and the provider processed afresh — plus, when the TTL had expired, the
shared prefix too. Estimates are scaled by the call's own calibration: the
provider's resolved context count over the estimate for the whole context,
clamped to 0.25–4×, so the figure tracks the provider's tokenizer rather than
the four-characters rule. The explanation quotes the same scaled figure.

`est_cost_lost_usd` is those tokens at the model's input rate *less* its cache
read rate, from [`--pricing`](../configuration.md#model-pricing) — present only
when the table names both for the model. A model the table cannot price
reports tokens and no money, and `summary.unpriced_breaks` counts such breaks,
so a total is never silently an undercount.

### When the provider disagrees

`cache_read` and `reported_hit` are the provider's own report, shown beside
every verdict. `summary.disagreements` counts calls where the two contradict
each other: a `stable` prefix with no cache read reported (the provider did not
cache — no cache breakpoint set, or a prefix below its minimum cacheable
length), or a read reported where nothing could have been served (a TTL
longer than the one assumed). The autopsy does not explain these away; it
counts them, and `summary.disagreement_advice` lists what to check.

## Pricing cached tokens

The same pricing keys fix cached-token arithmetic everywhere Traza derives a
cost. With `cache_read_per_mtok` and `cache_write_per_mtok` in a model's entry,
a call's priced cost counts cache reads and writes at their own rates under its
provider's convention — on top of the prompt for Anthropic, Bedrock and Vertex,
carved out of it for OpenAI and Azure OpenAI. Without them, nothing changes.
See [Model pricing](../configuration.md#model-pricing).

## From an agent: `xray_step`

With [`--mcp`](mcp.md), the `xray_step` tool returns a compact X-Ray: a
headline of counts in Traza's own words, then — inside the untrusted-telemetry
block — the model and run, the composition, the cache verdict with its diff,
what was dropped or edited since the previous call, repeats, and one line per
chunk. It takes `trace_id` and `span_id` (as `get_run_tape` lists them), or
`run_id` and `step` (a tape index). Everything the telemetry wrote — previews,
roles, model names, the explanation, which quotes recorded text — renders
inside the block; the result is clamped to `--mcp-max-result-bytes` with the
loss stated, the chunk list cut from its end first.

## Tenancy and erasure

Both routes take the usual `tenant=` selector and binding: a bound credential
reads only its own tenant, and naming another is `403 {"required": "unbound"}`.
The X-Ray is addressed by span, and span ids can repeat across tenants: in the
operator view (an unbound caller naming no tenant) a span id found in more than
one tenant is refused with `400` asking for `tenant=`, and a span found in
exactly one is read entirely under that tenant — its run, its history and its
payloads. Runs, spans and payloads are read under the store's erasure mask, so
an erased trace or session is simply absent, and nothing here keeps a copy of
anything.

## Limits, stated plainly

- **Estimates and heuristics are labelled.** Per-chunk tokens are estimates;
  staleness is the heuristic above; "near-identical" is a 90%-shared-text rule;
  the predecessor search is a heuristic about which call a conversation
  continues. A run with conversations that share most of their content may
  pair calls across them.
- **The provider's cache rules are not modelled** — minimum cacheable length,
  breakpoint placement, TTL refreshes on a hit, per-organization cache
  isolation. The verdicts describe the recorded prompt; `cache_read` and
  `disagreements` are where the provider's behaviour shows.
- **Byte-exactness is the recording's.** Instrumentation records messages, not
  the HTTP body the SDK sent; a difference the instrumentation normalized away
  is invisible, and one it introduced is reported. Structured values are
  compared in canonical key order, so a key-order change is visible only when
  it travelled as text.
- **Bounds.** One request reads its run's first 20,000 spans; a call past that
  is X-rayed alone (`step_index: null`, `provenance_partial: true`, cache
  `unknown`). A context is split into at most 2,000 chunks
  (`chunks_truncated`). Up to 64 MiB of offloaded payload is resolved for an
  X-Ray's history and as much again for the call itself, and 64 MiB across
  all of an autopsy's calls (`resolved_partial`). At most
  200,000 content digests are kept for provenance and 500,000 words for
  staleness (`provenance_partial`; staleness is then not judged at all), 32 MiB
  of distinct chunk text for previews and diffs (the autopsy's
  `texts_partial`), 256 dropped and 256 edited chunks and 64 duplicate groups
  listed, and an autopsy covers at most 2,000 model calls (`truncated`). The
  whole request runs inside one `--query-deadline-ms` budget and is refused
  with `400`, never answered partially, when it runs out.
- **Digests are identities, not commitments.** They are Traza's 128-bit hash,
  reported as 16 hex characters. A producer able to write its own telemetry can
  manufacture a collision; it would get a wrong answer about its own data in
  its own tenant, and nothing else. The autopsy confirms a digest match on the
  text whenever both texts are held.
- **Media is not estimated**, and an unrecognized message shape is an `other`
  chunk holding its canonical JSON rather than a guess at its parts.
