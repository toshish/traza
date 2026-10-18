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
    "est_tokens_lost": 10, "calibration": null, "est_cost_lost_usd": 0.000027,
    "explanation": "chunk 0 (system) changed in place since step 0: \"2026-10-14T09:00:01Z\" → \"2026-10-14T09:00:02Z\" (a timestamp); ~10 tokens of content step 0 already had were re-processed"}
}
```

Read top to bottom: the user's question and the model's tool call came in with
the first call; the tool result came from the tool step; the system prompt is
new in this call — because one second of clock in it changed, which is also
why the provider's cache served nothing. (`calibration` is `null` here: the
agent uses a tool whose definition the span did not record, so the provider's
96 tokens cover text the estimate cannot see, and the raw estimate stands.)

## See it run

[`examples/coding-agents/run.sh`](../../examples/coding-agents/README.md)
runs both routes on [imported](recorders.md) Claude Code transcripts. The
autopsy of one session names an expired TTL, priced, and a `/compact`
(`new_context`, `rewritten`); the X-Ray of the call before the compaction
lists its chunks with the step each came from, and the X-Ray of the call
after it lists, under `dropped_since_previous`, the instruction the
compaction took.

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

A tool call's text is `name(arguments)`: the arguments exactly as recorded
when they travelled as text (OpenAI's `arguments` string), canonical JSON when
they were recorded as structured JSON. A tool definition's text is likewise
its recorded text when it has one — a `gen_ai.tool.definitions` list recorded
as one string is split into each tool's own bytes — else canonical JSON. A tool
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
instead — role, kind, tool-call id and the chunk's text as shown above —
because a provider's cache matches bytes: the same content re-serialized with
its keys in another order is new to the cache even though it is not new
content. Text-recorded values (messages, tool-call arguments, tool
definitions) keep their recorded bytes here; a value the instrumentation
recorded as structured JSON no longer has a byte order to keep, so for it the
identity is its canonical form.

**Offloaded prompts are resolved.** A request offloaded to the payload store
(anything over `--payload-threshold-bytes`, 256 KiB by default — long agent
contexts usually are) is read back before it is chunked, under the caller's
tenant, and so are the run's earlier requests the analysis compares against —
the 32 calls a predecessor can be first, newest first, then older history
(read only for provenance and staleness) with what the budget has left.
`resolve=false` skips that: an offloaded request is then one opaque chunk,
`context_unavailable` says why, and the cache verdict is `unknown`.

The same holds for every part of a context, read separately: the request,
the tool definitions, and — for a call [recorded as a
delta](#conversations-recorded-as-deltas) — the earlier answer it continues
from. Whichever part stays a reference (the budget ran out, its bytes are
gone, or `resolve=false`) is chunked as its placeholder, which is not what
the model saw, so the context is reported as not read whole
(`context_unavailable`): it is not compared with its predecessor's (`unknown`),
not used to calibrate, and a chunk the previous call had that it lacks is
`unverified` rather than lost. What was not read is never evidence.

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
annotated — no tool step holds the same content, and the chunk is attributed
to the tool step whose `gen_ai.tool.call.id` matches the result's call id,
with `via: "tool_call_id"`: the latest such step before the call that first
carried the result, since a call id can repeat within a run (a retried call,
a framework that numbers calls per turn). A tool step that holds the same
content always wins over an id match. That is worth noticing on its own: the
model did not see what the tool said.

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
all, nothing is called stale — there is nothing to judge by. Nor is anything
when some model output of the history (an answer, or a tool's arguments) was
offloaded and could not be read: the words it said are unknown, and a result
it quoted would look unquoted. `provenance_partial` says so, as it does when
any value of the history stayed unresolved, since content first seen there
would read as introduced later.

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
  is an edit, not a lost instruction, however long it is.

Before a chunk is called lost, every chunk of this call that could be it
edited (the same role and kind, and a length within a tenth of it) is
compared with it. When that comparison could not be made — the old text was
not kept for lack of room (`texts_partial`), or the comparison bound was
reached — the dropped chunk is marked `"unverified": true`: it may have been
changed rather than lost. So is every dropped chunk when this call's context
was not read whole — cut at 2,000 chunks (`chunks_truncated`), or partly
unreadable (`context_unavailable`): a chunk not found in what was read may
sit in what was not. (Prepend one instruction to a 2,000-chunk history and
its last chunk falls past the cut; it is listed, unverified, not lost.) A
chunk found edited is an edit either way. When the *previous* call's context
was cut (`cache.chunks_truncated`), only its first 2,000 chunks are checked.
`xray_step` counts only verified losses as lost instructions.

## Conversations recorded as deltas

Some recorders write each model call's *new* messages only, not its whole
context again — a transcript of a long session would otherwise grow with the
square of its length. The [recorders](recorders.md) do this for Claude Code
and Codex, and mark it on every model call:

- `gen_ai.conversation.id` names the conversation the call belongs to: the
  session for a main transcript, `<session>/<agent id>` for a Claude Code
  sub-agent, the thread for Codex.
- `traza.input.delta: true` says the call's `gen_ai.input.messages` holds only
  the messages sent since the conversation's previous call. The first call
  after a compaction is marked `false`: it starts over, and its input is all
  it saw.

The X-Ray and the Cache Autopsy read such a call as the **whole context it
was**: the conversation's previous context, then what that call answered,
then the call's own messages — rebuilt by reading the run's model calls in
step order. An X-Ray of the fortieth call of a Claude Code session shows the
task, every answer and every tool result of the forty, with each chunk's
provenance and age, not the one tool result the transcript recorded for it.
A call whose delta is empty (a retry) is the conversation so far plus the
last answer. Any other producer can use the same two attributes.

A conversation is identified by its **whole** id, compared through a SHA-256
digest of it — never a prefix, so two long ids that begin alike are two
conversations, and neither continues the other. A delta call that names no
conversation that can be read (none, or an id so long it was offloaded) has
nothing to be rebuilt from, and says so (*could not be rebuilt*) rather than
passing its delta for its whole context. Gaps carry forward too: when an
earlier call of the conversation, or the answer it gave, could not be read
(offloaded and unresolved), every later delta call of it is reported with its
context unavailable — it carries that gap — rather than as a whole context
with a placeholder inside.

A call that names a conversation **pairs with the nearest earlier call of
that conversation**, whatever the two share, and never with a call of
another conversation (or of none). After a compaction that is the call it
compacted — so `dropped_since_previous` lists everything the compaction took,
the instructions flagged — and the autopsy calls it `new_context`,
`rewritten`. A sub-agent's first call is the first of its own conversation.

Bounds and what they cost: one request keeps at most 64 conversations'
running contexts and 64 MiB of their chunk text; past either the least
recently continued conversation is dropped, and a later delta call of it is
reported with its context unavailable (*could not be rebuilt*) rather than
as a context it was not. A rebuilt context is held to the 2,000-chunk bound
like any other (`chunks_truncated`). The X-Ray reads the run's earlier calls
twice when it rebuilds — once in order to rebuild the call and the comparison
window, once as recorded for provenance — inside the same query deadline and
payload budgets. A call past a run's first 20,000 spans is read with the
20,000 spans that end at it (below): a conversation already under way before
them is rebuilt from its first compaction inside them on, and a delta call of
it before that compaction says it *could not be rebuilt*, its conversation
having begun before the spans read. Measured on one real Claude Code session of 1,482
model calls (26 MB of transcript, release build, a laptop): the X-Ray of its
last call rebuilt 1,002 chunks in 0.5 s, and the autopsy of all 1,482 calls
took 1.4 s, finding its two compactions as `rewritten`.

What a transcript does not record, the rebuilt context cannot show: Claude
Code and Codex transcripts carry neither the system prompt nor the tool
definitions, so a rebuilt context is the conversation only. A `stable`
verdict therefore says the *recorded* prefix held; when the provider reports
no cache read for such a call, the autopsy counts it among `disagreements` —
the unrecorded part changed, or the cache entry was evicted.

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
             "est_tokens_lost": 10, "est_cost_lost_usd": 0.000027, "disagreements": 0,
             "culprits": [{"cause": "volatile_content", "change": "edited", "role": "system",
                           "volatile": "timestamp", "breaks": 1, "est_tokens_lost": 10,
                           "est_cost_lost_usd": 0.000027, "example_step_index": 2,
                           "example": {"offset": 34, "before": "2026-10-14T09:00:01Z",
                                       "after": "2026-10-14T09:00:02Z"},
                           "advice": "A clock value sits inside the cached prefix. Move the time to the end of the prompt (after the last cache breakpoint), or round it to a coarser unit such as the day."}],
             "culprits_truncated": false}}
```

`culprits` groups the breaks by shape, costliest first — by estimated cost
when every shape could be priced, else by estimated tokens lost, since
neither a mix of priced and unpriced shapes nor tokens across differently
priced models ranks by money — the kind of headline that reads *a timestamp in
the system prompt broke the cache on 47 of 48 calls*. Each carries `advice`:
fixed guidance chosen by the shape (a lookup table in the code, not generated
text), such as moving a clock value after the cached prefix, appending
retrieved documents after the history rather than ahead of it, or serializing
tool output with sorted keys. At most ten are listed; `culprits_truncated`
says when there were more.

### The predecessor

A provider's prompt cache serves the longest previously-seen **prefix** of a
prompt, byte for byte, while the entry lives. So each call is compared with the
call before it **in the same conversation** — its predecessor — chosen among
the previous 32 model calls of the same tenant:

- A candidate **qualifies** when its leading chunks match this call's past the
  tool definitions — the same system prompt, or the same first turn — or,
  failing that, when the two share at least half of either context's chunks
  (which is what still pairs a context whose tool definitions were
  re-ordered). Leading chunks also match when they are near-identical, so a
  system prompt with a clock in it does not hide the conversation it belongs
  to. A compaction keeps the system prompt and replaces the history, so it
  finds the call it compacted whatever share of the context it replaced;
  sharing only the tool catalog is not enough.
- Among qualifying candidates, the one whose matching lead is **longest** wins,
  then the one sharing the most chunks, then the nearest. A sub-agent that
  opens with its parent's system prompt therefore does not capture the
  parent's next call, which continues a longer prefix.
- When both calls name an agent — `gen_ai.agent.name` on the call, else the
  name of the nearest enclosing agent step — calls of **different agents never
  pair**.
- When a call names its conversation (`gen_ai.conversation.id`), none of the
  above applies: its predecessor is the nearest earlier call of the same
  conversation, and nothing else ([conversations recorded as
  deltas](#conversations-recorded-as-deltas)).

A call with no predecessor is `new_context`. The explanation says which case
it is: *the first model call of the run*, *no earlier call among the
previous 32 shares this call's leading chunks or half of its chunks* — a new
conversation, or one whose previous call lies further back — or, for a call
that names its conversation, *no earlier call among the previous 32 belongs
to this call's conversation*. If a nearer
earlier call of the tenant could not be read (not recorded, or offloaded and
unresolvable) before any qualifying one was found, the verdict is `unknown`
rather than a pairing across the gap.

### Causes

The two contexts are compared chunk by chunk from the start, on exact
identity; `prefix_chunks_shared` is how far they agree.

| `cause` | When | `first_divergent_chunk.change` |
|---|---|---|
| `stable` | The predecessor's whole context is an unchanged prefix of this one, within the TTL | — |
| | A **new turn**: everything up to the predecessor's last chunk is unchanged, this call puts new content (or an edit at its very end) in that chunk's place, and nothing the predecessor had follows — a stateless pipeline's next input, a replaced last message. The cache served all it could | `replaced` or `edited` |
| `ttl_expired` | The prefix is intact (or a new turn), but the TTL had passed since the predecessor began — unless the provider reports having served it (below), which makes the call `stable` | — or `replaced` / `edited` |
| `volatile_content` | A chunk inside the prefix changed in place — the same role and kind, at most a tenth of its text differing, verified on both texts — with content the predecessor had after the change | `edited` |
| | A chunk inside the prefix was swapped for different content while what followed it survives (or its text was not kept, so an edit could not be confirmed) | `replaced` |
| | A new chunk sits ahead of content the predecessor already had | `inserted` |
| `reordered` | The same content in different bytes (key order, spacing), or under another role | `reserialized` |
| | Both differing chunks exist in both contexts, in other places | `moved` |
| `new_context` | No predecessor | — |
| | A chunk was removed from inside the prefix (pruning) | `removed` |
| | Less than half of what followed the divergence survives (compaction, truncation) | `rewritten` |
| `unknown` | This context is a strict prefix of the predecessor's: whether a shorter prompt hits depends on where the provider's cache breakpoints sat, which no trace records | `shrunk` |
| | This call's or the predecessor's context cannot be read whole (`context_unavailable`) | — |
| | A context was cut at 2,000 chunks and what lies past the cut could change the verdict (below) | — or as compared |

**A context cut at 2,000 chunks** (`chunks_truncated` on the verdict, and on
the autopsy) is compared as far as it was split, and what lies past the cut
is never taken to match. A chunk found is found, but one not found may sit
past the cut, so a verdict is asserted only when nothing there could change
it: a predecessor read whole that this call starts with is still `stable`,
and an insertion ahead of history or a move — decided by what was found — is
still named. A verdict that needs the rest — that the whole prefix held, that
nothing the predecessor had follows (a new turn), that most of it is gone (a
rewrite), that a chunk was removed — is `unknown`, with nothing counted lost
and the explanation saying where the compared chunks diverged and which
context was cut; `first_divergent_chunk`, when present, describes the chunks
compared. A verdict that stands counts lost tokens among the compared chunks
only.

The TTL defaults to 300 seconds — Anthropic's default cache lifetime and the
low end of OpenAI's — and `ttl_seconds=` sets it per request (up to a day) for
deployments on longer-lived caches. `gap_ns` is the time since the predecessor
*began*, which is when its prompt entered the cache. A structural divergence
outranks an expired TTL as the cause; the explanation mentions both.

The TTL is an assumption; the provider's report is evidence. When the prefix
was carried intact past the TTL and the call reports reading at least half of
what its predecessor reported as its whole context from the cache — both
counts in the provider's own tokens — the cache plainly outlived the
assumption: the verdict is `stable`, nothing is counted lost, and the
explanation says the cache outlived the assumed TTL. Real producers run longer
caches than five minutes: on one real Claude Code session, 15 of the 25 calls
a five-minute TTL called expired had read their whole prefix from the cache.
A smaller read (a long-lived system prompt alone, say) leaves `ttl_expired`
standing, and no structural verdict ever yields to a reported count.

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
served and the provider processed afresh: the chunks after the divergent one
that the predecessor had, and, for an edited chunk, its own text after the
change (a clock at the top of a long system prompt costs the whole prompt
below it) — plus, when the TTL had expired, the shared prefix too. A new turn
loses nothing, which is why it is not a break.

`calibration` is the call's provider-reported context count over the
estimate for the whole context, clamped to 0.25–4×, and `est_tokens_lost` is
scaled by it so the figure leans toward the provider's tokenizer rather than
the four-characters rule. It is applied only when the estimate covers what the
provider counted: not when the context holds media (estimated at zero), uses
tools without recording their definitions, was cut at the chunk cap, or was
not read whole (an offloaded part — definitions, or an earlier answer of a
delta-recorded conversation — left as a placeholder) — the ratio would then
mostly measure what was not recorded — and `calibration` is `null` and the
raw estimate stands. Even when applied, the ratio absorbs
anything the provider adds that no span records (injected system text,
formatting tokens). The explanation quotes the figure reported.

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
length), or a read reported where nothing could have been served. The
autopsy does not explain these away; it counts them, and
`summary.disagreement_advice` lists what to check. The one report it acts on
is a read of most of an intact prefix past the assumed TTL (above): that call
is `stable`, and agrees with the provider.

## Pricing cached tokens

The same pricing keys fix cached-token arithmetic everywhere Traza derives a
cost. With `cache_read_per_mtok` and `cache_write_per_mtok` in a model's entry,
a call's priced cost counts cache reads and writes at their own rates under its
provider's convention — on top of the prompt for Anthropic, Bedrock and Claude
on Vertex, carved out of it for OpenAI, Azure OpenAI and Gemini on Vertex
(Vertex is decided by the model's family; any other Vertex model keeps the
plain arithmetic). Without them, nothing changes.
See [Model pricing](../configuration.md#model-pricing).

## From an agent: `xray_step`

With [`--mcp`](mcp.md), the `xray_step` tool returns a compact X-Ray: a
headline of counts in Traza's own words, then — inside the untrusted-telemetry
block — the model and run, the composition, the cache verdict with its diff,
what was dropped or edited since the previous call, repeats, and one line per
chunk. It takes `trace_id` and `span_id` (as `get_run_tape` lists them), or
`run_id` and `step` (a tape index) — either way the run is read once, under
one query deadline; with a credential that sees every tenant, a run present in
more than one is refused rather than guessed at. Everything the telemetry
wrote — previews, roles, model names, the explanation, which quotes recorded
text — renders inside the block; the headline counts only verified lost
instructions and says how many losses are unverified. The result is clamped to
`--mcp-max-result-bytes` with the loss stated, the chunk list cut from its end
first, and the note says whether the summary rows survived.

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
  continues. Conversations that open with the same system prompt or share
  most of their content can pair across each other unless their calls name
  different agents: the first call of a sub-agent that reuses its parent's
  system prompt pairs with the parent's latest call, and lists the parent's
  turns as dropped. A compaction that rewrites the system prompt too leaves
  nothing to link it to the call it compacted.
- **The provider's cache rules are not modelled** — minimum cacheable length,
  breakpoint placement, TTL refreshes on a hit, per-organization cache
  isolation. The verdicts describe the recorded prompt; `cache_read` and
  `disagreements` are where the provider's behaviour shows.
- **Byte-exactness is the recording's.** Instrumentation records messages, not
  the HTTP body the SDK sent; a difference the instrumentation normalized away
  is invisible, and one it introduced is reported. Values recorded as
  structured JSON (Anthropic's `tool_use` input, indexed tool-definition
  attributes, a provider body's `tools`) are compared in canonical key order,
  so a key-order change in them is invisible; text-recorded arguments and
  definitions keep their bytes, offloaded or not.
- **Bounds.** One request reads 20,000 of its run's spans: the first 20,000,
  or — for a call past them, such as the latest call of a long session — the
  20,000 that end at it. Its `step_index` (and every index it names) still
  counts the whole run's steps, and `provenance_partial` is `true`: a chunk
  first seen before those spans is not traced to its step. (A call that is
  not among its run's spans at all is X-rayed alone: `step_index: null` here
  and in `cache`, cache `unknown`.) A context is split into at most
  2,000 chunks (`chunks_truncated` on the X-Ray, on a verdict when either
  context compared was cut, and on the autopsy; past the cut nothing is taken
  to match or to be missing — see [Causes](#causes) and
  [compaction loss](#what-the-previous-call-had-compaction-loss)). Up to
  64 MiB of offloaded payload is
  resolved for the call under an X-Ray and as much again for its history — the
  32 calls a predecessor can be first, newest first — and an autopsy resolves
  up to 64 MiB per call and 512 MiB across all its calls (`resolved_partial`;
  a context a value left unresolved is not read whole, `context_unavailable`).
  These are bounds on reading work, not on response size, which is why they
  are not `--tape-max-bytes` (the cap on content a tape *returns*); the query
  deadline bounds them as well. At most 200,000 content digests are kept for
  provenance and 500,000 words for staleness (`provenance_partial`; staleness
  is then not judged at all), one request remembers 64 running
  conversations and 4,096 dropped ones, each by the SHA-256 of its id, and
  32 MiB of distinct chunk text for previews
  and diffs — the examined call's and the comparison window's, released as
  calls leave the window (`texts_partial`). Pairing lost chunks with edited
  ones compares at most 64 MiB of text and 4,096 pairs (`unverified` past it).
  At most 256 dropped and 256 edited chunks, 64 duplicate groups and 10
  culprits are listed (`dropped_truncated`, `edited_truncated`,
  `duplicates_truncated`, `culprits_truncated`), and an autopsy covers at most
  2,000 model calls (`truncated`). The whole request runs inside one
  `--query-deadline-ms` budget and is refused with `400`, never answered
  partially, when it runs out.
- **Digests are identities, not commitments.** They are Traza's 128-bit hash,
  reported as 16 hex characters. A producer able to write its own telemetry can
  manufacture a collision; it would get a wrong answer about its own data in
  its own tenant, and nothing else. The autopsy confirms a digest match on the
  text whenever both texts are held.
- **Media is not estimated**, and an unrecognized message shape is an `other`
  chunk holding its canonical JSON rather than a guess at its parts.
