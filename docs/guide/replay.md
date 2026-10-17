# Replay: tapes, the replay proxy, diffs and recall

Traza already records what an agent did. This guide covers what you can do
with that recording afterwards:

- read a run as a **tape** — its model calls, tool calls and agent
  invocations in order, with what went into each and what came out;
- **replay** a run: point the agent's OpenAI or Anthropic SDK at Traza and
  re-run it, with every model answer served from the tape — zero model calls;
- **diff** two runs to find the first step where they parted, and what kind of
  difference it was;
- **recall** earlier runs that dealt with the same thing, successful ones
  first.

All of it runs in the one `traza-server` process. Traza never calls a model:
a replay answers *in place of* the provider, from what was recorded. Route
shapes are in the [HTTP API reference](http-api.md#runs-tapes-replays-diffs-and-recall).

## See it run

[`examples/replay/run.sh`](../../examples/replay/README.md) records a run of
an agent written against the OpenAI chat-completions wire — a model call that
asks for a tool, the tool, a model call that answers — with a scripted
stand-in model, as OpenTelemetry GenAI spans. Then it stops the model, creates
a replay, and runs the same agent code with `OPENAI_BASE_URL` set to the
replay: both answers come back identical, every model call goes to the
replay, and the replay reports `served` equal to the run's model steps. A
changed system prompt is refused with `replay_divergence` at step 0, and
`GET /v1/diff` names where a run with that prompt parted from the original.
It takes about two seconds and exits non-zero if any claim fails.

## Runs and tapes

A **run** is one unit of agent work. Its id is the span's session id when it
has one (`session.id`, `gen_ai.conversation.id`, or OpenLLMetry's
`traceloop.association.properties.session_id`), otherwise `trace:<trace_id>`
([run identity](../llm-semantics.md#run-identity)). Every route below takes a
run id; a bare trace is always reachable as `trace:<trace_id>`.

A **tape** is the run's *steps* — the spans whose step kind is `llm`, `tool`
or `agent` ([step kinds](../llm-semantics.md#step-kind)) — in start order.
Workflow wrappers, HTTP spans and retrievals are context around steps, not
steps, so they are not on the tape.

The examples in this guide use a weather agent instrumented with OpenLLMetry.
Its run `weather-7` is four spans: a workflow wrapper, a model call that asks
for the `get_weather` tool, the tool, and a model call that answers.

```sh
curl -s localhost:8080/v1/tapes/weather-7
```

```json
{
  "run": "weather-7",
  "tenant": null,
  "spans_examined": 4,
  "truncated": false,
  "steps": [
    {
      "index": 0,
      "kind": "llm",
      "name": "openai.chat",
      "model": "gpt-4o-2024-08-06",
      "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
      "span_id": "llm-1",
      "parent_span_id": "wf",
      "service": "weather-agent",
      "start_ns": 1791964800010000000,
      "duration_ns": 390000000,
      "status": "ok",
      "request": [
        {"role": "system", "content": "You are a weather assistant."},
        {"role": "user", "content": "What's the weather in Paris?"}
      ],
      "response": [
        {"role": "assistant", "finish_reason": "tool_calls",
         "tool_calls": [{"id": "call_w1", "name": "get_weather", "arguments": {"city": "Paris"}}]}
      ],
      "request_digest": "8b9a07d4aac0a133c789406dfccd9477618c1049ca0255f58b262fc72fca3d8b",
      "response_digest": "bc0a9a93dfc45c687262d0e46fceb83a1c7c92d362ccb045210310872c51d35f",
      "fingerprint": "llm:gpt-4o-2024-08-06#8b9a07d4aac0a133",
      "tokens": {"input": 82, "output": 17, "cache_read": null, "cache_write": null}
    },
    {
      "index": 1,
      "kind": "tool",
      "name": "get_weather.tool",
      "tool": "get_weather",
      "request": {"city": "Paris"},
      "response": {"celsius": 12, "forecast": "rain"},
      "fingerprint": "tool:get_weather#6e1e312d537bc71b",
      "tokens": {"input": null, "output": null, "cache_read": null, "cache_write": null},
      "…": "trace, span, timing, status and digests as above"
    },
    {"index": 2, "kind": "llm", "…": "the model call that answers"}
  ]
}
```

(The response is abbreviated where marked; the server returns keys in sorted
order.) Requests and responses are read with the same precedence the store
uses everywhere — OTel GenAI `gen_ai.input.messages`, OpenLLMetry's indexed
`gen_ai.prompt.{i}.*`, OpenInference's `llm.input_messages.{i}.message.*`,
Traza's native events — and the indexed layouts all fold to one message shape.
A token count a span did not report is `null`, not `0`.

**Offloaded values.** A prompt or completion longer than
`--payload-threshold-bytes` (256 KiB by default) is stored as a
`{"$payload": "sha256/…"}` reference. `?resolve=true` inlines references, up
to [`--tape-max-bytes`](../configuration.md#server-flags) of payload content
per tape (8 MiB by default). The cut is a prefix — the first reference that
does not fit, and every one after it, stays a reference — and the tape then
says `"resolved_partial": true`. Digests and fingerprints always describe the
stored form, so a tape read with and without `resolve` agrees on every
identity it reports. Resolution spends the request's one
[`--query-deadline-ms`](../configuration.md#server-flags) budget — reading the
run and every payload together — so a run whose payloads cannot all be read
within it is refused with `400` rather than allowed to run past it. A payload
repeated across steps is probed for tenant reachability once per request.

A model call that recorded `gen_ai.system_instructions` beside an offloaded
`gen_ai.input.messages` is stored as both: the system message, then the
reference — `[{"role": "system", …}, {"$payload": "sha256/…"}]` — so the
digest covers the system prompt too. Resolved, the two combine as they would
have inline: the system message, then the list's own messages.

## Replaying a run with zero model calls

Replay re-executes your agent against its own recording. The agent runs for
real — your code, your prompts, your control flow — but every model call it
makes is answered by Traza with the next recorded model step, in the shape the
provider's SDK expects.

**1. Record the run with content capture on.** A replay can only serve answers
that were recorded. With OpenLLMetry that means `TRACELOOP_TRACE_CONTENT`
left at its default (`true`); a model step recorded without its completion is
refused at step 2, naming the step.

**2. Create a replay of the run.**

```sh
curl -s -X POST localhost:8080/v1/replays \
  -H "Authorization: Bearer $TRAZA_TOKEN" \
  -d '{"run": "weather-7"}'
```

```json
{
  "id": "rp_ac879985abef9cba12bb342bdc3b8bd1",
  "run": "weather-7",
  "mode": "strict",
  "steps": 3,
  "llm_steps": 2,
  "expires_at_ns": 1792065649987347000,
  "unreadable_tool_steps": [],
  "base_urls": {
    "openai": "/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/openai/v1",
    "anthropic": "/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/anthropic"
  }
}
```

The replay is a snapshot of the tape taken now. It lives for one hour from
creation, in memory; a restart forgets it, and creating another is just
another read. Any valid credential may create one — an `ro` token is enough,
because nothing in the store changes.

A replay holds only what it serves: each model step's answer and each tool
step's arguments and result, read from payload files where they were
offloaded, up to [`--tape-max-bytes`](../configuration.md#server-flags)
between them. Recorded *requests* are not held — each is read, digested for
strict matching, and dropped — so a long conversation whose prompt grows at
every step costs the budget nothing for its prompts. A tool step whose value
could not be read, or that recorded no result at all, is listed in
`unreadable_tool_steps` (see
[tools](#tools-still-execute-live--unless-you-replay-them-too)). A run with
more than 20,000 spans is refused (`409`) rather than replayed as its
beginning, which would end the agent's run early as if the recording stopped
there.

**3. Point the SDK at it and run the agent.**

```sh
export OPENAI_BASE_URL=http://localhost:8080/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/openai/v1
export OPENAI_API_KEY=$TRAZA_TOKEN
python weather_agent.py
```

For an agent on the Anthropic SDK:

```sh
export ANTHROPIC_BASE_URL=http://localhost:8080/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/anthropic
export ANTHROPIC_API_KEY=$TRAZA_TOKEN
```

The OpenAI SDK sends its key as `Authorization: Bearer`; the Anthropic SDK
sends `x-api-key`, which Traza accepts as the credential on the routes under
`/v1/replays/` (and nowhere else), looked up like any bearer token — a
`TRAZA_TOKENS` entry or a credential minted at runtime. On an open server (no
credential of either kind) any non-empty key works — the SDKs refuse to start
without one.

The agent's first call — `POST …/openai/v1/chat/completions` with the system
and user messages — receives the recorded answer as an ordinary
`ChatCompletion`:

```json
{
  "id": "chatcmpl-trazaac879985abefs0",
  "object": "chat.completion",
  "created": 1791964800,
  "model": "gpt-4o-2024-08-06",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": null,
      "refusal": null,
      "tool_calls": [{"id": "call_w1", "type": "function",
                      "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]
    },
    "logprobs": null,
    "finish_reason": "tool_calls"
  }],
  "usage": {"prompt_tokens": 82, "completion_tokens": 17, "total_tokens": 99},
  "system_fingerprint": null
}
```

The model, `created` time, tool-call ids and token usage are the recording's.
An id the recording did not carry (here, the response id) is made up from the
replay id and the step; a recording with no model gets the request's `model`
back, or `"unknown"`. A model name or response id longer than 256 bytes —
recorded or requested — is treated as absent: a stream repeats both in every
chunk, and no provider's come close. The agent then runs `get_weather`
itself, sends the result back, and receives the recorded final answer.

**4. Check what was served.**

```sh
curl -s localhost:8080/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1 \
  -H "Authorization: Bearer $TRAZA_TOKEN"
```

```json
{
  "id": "rp_ac879985abef9cba12bb342bdc3b8bd1",
  "run": "weather-7",
  "mode": "strict",
  "served": 2,
  "remaining": 0,
  "diverged": null,
  "calls": [
    {"step": 0, "matched": true, "at_ns": 1792062055501840000},
    {"step": 2, "matched": true, "at_ns": 1792062063623225000}
  ],
  "unreadable_tool_steps": [],
  "expires_at_ns": 1792065649987347000
}
```

Both recorded model steps were served and both requests matched the
recording. Nothing was sent to a model provider: the replay routes make no
outbound request of any kind. A third model call would get `410` with
`{"error": {"type": "replay_exhausted", …}}`.

### Streaming

With `"stream": true` the answer comes back as server-sent events —
`chat.completion.chunk` events ending in `data: [DONE]` for OpenAI (plus a
usage chunk when `stream_options.include_usage` is set), and `message_start`,
`content_block_start` / `_delta` / `_stop`, `message_delta`, `message_stop`
for Anthropic, with tool calls as `tool_calls` argument deltas and
`input_json_delta` respectively:

```text
data: {"choices":[{"delta":{"content":"","refusal":null,"role":"assistant"},"finish_reason":null,"index":0,"logprobs":null}],"created":1791964800,"id":"chatcmpl-trazaac879985abefs2","model":"gpt-4o-2024-08-06","object":"chat.completion.chunk","system_fingerprint":null}

data: {"choices":[{"delta":{"content":"It's raining in "},"finish_reason":null,"index":0,"logprobs":null}],…}

data: {"choices":[{"delta":{"content":"Paris, 12°C. "},"finish_reason":null,"index":0,"logprobs":null}],…}

data: {"choices":[{"delta":{"content":"Take an "},"finish_reason":null,"index":0,"logprobs":null}],…}

data: {"choices":[{"delta":{"content":"umbrella."},"finish_reason":null,"index":0,"logprobs":null}],…}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0,"logprobs":null}],…}

data: [DONE]
```

**The stream is reconstructed from the stored text.** No instrumentation
records how a provider chunked its stream, so Traza re-chunks: windows of 16
characters (wider for long text), cut back to just after whitespace. The
events are what the SDKs parse, and the text they reassemble is exactly the
recorded text; the chunk boundaries are not the provider's.

### Strict and loose matching

Steps are served in the order they were recorded. In **strict** mode (the
default) each request must also *be* the recorded request: its messages are
normalised into the message shape step facts use — role, content, tool-call
id, and each tool call's id, name and arguments, with JSON-shaped text read as
JSON — digested, and compared with the recorded step. The recorded side is
normalised the same way, so a run recorded with OTel `parts` messages, with
raw provider-shaped messages, or with offloaded content compares by what it
said rather than how its instrumentation spelled it. That includes
`gen_ai.system_instructions` recorded beside an offloaded
`gen_ai.input.messages`: the two are combined once the list is read, exactly
as they would have been inline. For the indexed layouts and native events the
recorded digest is exactly the tape's `request_digest`.

A request that does not match is refused and **does not consume the step**:

```text
HTTP/1.1 409 Conflict
Content-Type: application/json
x-should-retry: false

{"error":{"type":"replay_divergence","step":2,
  "message":"this request's messages do not match recorded step 2: the agent diverged from the recording, so replaying the recorded answer would answer a different question",
  "expected_digest":"6db28425e9e18cdbf0bb3dd925d6afa9352b5f91e8a33438e5d4a29f03993f9d",
  "got_digest":"cd1ff438bb759f43d89b1089fc5ca10b67a08a6311fee65adc9d4bee844c8fc3"}}
```

A divergence means your agent asked a different question than it asked when
the run was recorded — a changed prompt, a different tool result, a new
branch — and serving the recorded answer would answer the wrong question. The
replay remembers the first divergence (`"diverged"` in its status). Both SDKs
retry a `409` by default; `x-should-retry: false` tells them not to, because
the same request cannot start matching.

**Loose** mode (`{"run": "…", "mode": "loose"}`) serves the steps in order
whatever is asked, and records whether each call matched. Use it when the
recording cannot be compared, or when you want to drive an agent down the
recorded path regardless. A model step whose prompt was not recorded as a
message list is refused in strict mode, naming the step: a bare prompt — text,
or a JSON object such as a whole request body in OpenInference's
`input.value` — inline or offloaded, with or without
`gen_ai.system_instructions` beside it, even when the object reads like a
message; and a list holding anything that is not a message (an element with
no role, content, tool call or tool result), which would otherwise compare as
if that element were not there. In loose mode such a step is served, and its
calls are recorded as not matched.

What strict mode compares is the **messages only** — not the model name,
`tools`, temperature or any other parameter. A request with the recorded
messages and a different model is served.

### Tools still execute live — unless you replay them too

A replay answers *model* calls. When the replayed model asks for a tool, your
agent runs the real tool, with its real side effects. For a tool whose result
you want replayed, take it from the recording instead:

```sh
curl -s -X POST localhost:8080/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/tools/next \
  -H "Authorization: Bearer $TRAZA_TOKEN" \
  -d '{"tool": "get_weather"}'
```

```json
{"step": 1, "tool": "get_weather", "arguments": {"city": "Paris"}, "result": {"celsius": 12, "forecast": "rain"}}
```

Each call returns the next recorded tool step not yet served (the next one
with that name, when `tool` is given), on a cursor of its own, independent of
the model steps; `410` when none is left. A step whose arguments or result was
offloaded and could not be read — erased, swept by retention, or past
`--tape-max-bytes` — is listed in the replay's `unreadable_tool_steps` and
answered `409` with `"type": "tool_unreadable"` instead of a payload reference
dressed up as the result; so is a step that recorded no result at all
(content capture was off), instead of a `null` your harness would take for
what the tool returned. A result recorded as empty — `""` or `null` — is a
result, and is served as recorded. A refused step is not consumed, because
moving past it would hand your harness the next step's result as this one's.
Wiring it in is a harness decision — for example, a wrapper your tool
dispatcher calls instead of the tool when a `REPLAY_ID` is set. Traza does not
intercept tool execution.

### Erasure reaches replays

A replay is a copy taken out of the store, so an
[erasure](http-api.md#post-v1erasures) has to reach it. Every call through a
replay first checks whether an erasure recorded since the replay was created
covers anything it holds — a span, trace or session its answers or tool
records came from, their tenant, or a payload they referenced. If one does,
the replay is withdrawn: its content is dropped, and that call and every
later one answer `410` with `"type": "replay_erased"` and the erasure's id,
until the replay would have expired. A withdrawn replay frees its place
among the live ones at once — an erasure does not lock a tenant out of
replays for an hour — and withdrawn replays are bounded on their own: each
tenant's share keeps at most 64 of them (the registry 1,024), and past that
the one closest to expiring is forgotten, its id answering `404` from then
on. The check applies from the moment the erasure is recorded, before its
purge runs, exactly as the store's own reads do; when nothing has been erased it is one comparison. Withdrawn content
leaves memory at the replay's next call — a replay nobody calls again keeps
its copy until it expires, at most an hour later.

## Comparing two runs

`GET /v1/diff?a=<run>&b=<run>` aligns two runs' steps and names the first
place they diverge. Here the second run asked about Lyon instead of Paris:

```sh
curl -s "localhost:8080/v1/diff?a=weather-7&b=weather-8"
```

```json
{
  "a": {"run": "weather-7", "steps": 3, "cost_usd": 0.0, "tokens": 233,
        "duration_ns": 1000000000, "errors": 0, "outcome": "success", "truncated": false},
  "b": {"run": "weather-8", "steps": 3, "cost_usd": 0.0, "tokens": 233,
        "duration_ns": 1000000000, "errors": 0, "outcome": "success", "truncated": false},
  "aligned": [
    {"op": "changed", "why": "input", "a": 0, "b": 0,
     "fingerprint_a": "llm:gpt-4o-2024-08-06#8b9a07d4aac0a133",
     "fingerprint_b": "llm:gpt-4o-2024-08-06#1ae84ec741143bc4"},
    {"op": "changed", "why": "input", "a": 1, "b": 1,
     "fingerprint_a": "tool:get_weather#6e1e312d537bc71b",
     "fingerprint_b": "tool:get_weather#ac8cc520a3295a36"},
    {"op": "changed", "why": "input", "a": 2, "b": 2,
     "fingerprint_a": "llm:gpt-4o-2024-08-06#6db28425e9e18cdb",
     "fingerprint_b": "llm:gpt-4o-2024-08-06#592c7b1d345f6104"}
  ],
  "first_divergence": {
    "a": 0, "b": 0, "kind": "input",
    "explanation": "step 0 of a and step 0 of b (llm:gpt-4o-2024-08-06) received different input: request 8b9a07d4aac0 in a, 1ae84ec74114 in b"
  },
  "delta": {"cost_usd": 0.0, "tokens": 0, "duration_ns": 0, "errors": 0, "steps": 0},
  "truncated": false
}
```

Steps are aligned by longest common subsequence over their **signatures** —
`kind:name`, the fingerprint without its input hash — so an extra or missing
step shifts nothing after it. Each aligned pair is then classified:

| What differs | `op` | `why` |
|---|---|---|
| nothing: same signature, input, output and configuration | `same` | — |
| the input to the same step | `changed` | `input` |
| the output, for the same input | `changed` | `decision` |
| the model, or any `gen_ai.request.*` parameter, of a model call | `changed` | `config` |
| a step only one run has | `insert` / `delete` | `structure` |

A model swap changes the signature (`llm:gpt-4o` is not `llm:gpt-4o-mini`), so
it is detected between the aligned steps: model calls that both name a model
and sit in the same unaligned stretch are paired as one `config` change.

`cost_usd` is the run's metered cost, or its cost priced from `--pricing` —
`0.0` here because these spans carry neither. Each side is compared up to its
first 5,000 steps; past that the diff says `"truncated": true`, and its tail
may report steps as missing that the cut removed. Each side is also read up to
its first 50,000 spans: a side longer than that says `"truncated": true` in
its own summary, its cost, tokens, duration and errors describe those first
spans, and its outcome is `unknown` — how a run ended cannot be read from its
beginning. A step's status is not compared: a step that errored with the same
input and output digests as a successful one is `same` — read `errors` and
the outcome for that.

Reading both runs, extracting their steps, aligning them, classifying each
aligned pair and explaining the first divergence all spend the request's one
[`--query-deadline-ms`](../configuration.md#server-flags) budget. The
alignment is quadratic in steps — two 5,000-step runs that share nothing
compare every pair — so it compares signatures as interned ids, whatever the
length of the names inside them, and checks the budget as it goes; so do
the classification, which compares each pair's configuration whole, and the
explanation, which compares every parameter of the divergent pair to count
how many differ — both check it while they compare, so neither many
parameters nor one large one can keep a comparison running past it. A pair
it cannot diff within the budget is refused with `400` rather than left
running, and so is one whose diff finished after the budget ran out.

## Recall: has this been done before?

`GET /v1/recall?q=<words>` finds earlier runs whose recorded text contains
every word of the query — whole words, any order, the same matching as
[`content=` search](http-api.md#content-search-content) — and ranks them:
successful runs first, then the runs with more matching spans, then the most
recent.

```sh
curl -s "localhost:8080/v1/recall?q=weather+Paris"
```

```json
{
  "runs": [{
    "run": "weather-7",
    "tenant": "",
    "outcome": "success",
    "outcome_source": "derived",
    "score": 3,
    "last_end_ns": 1791964801000000000,
    "steps": 3,
    "cost_usd": 0.0,
    "skeleton": ["llm:gpt-4o-2024-08-06", "tool:get_weather", "llm:gpt-4o-2024-08-06"],
    "skeleton_truncated": false,
    "truncated": false
  }],
  "truncated": false
}
```

The **skeleton** is what the run did without quoting what it said: its step
signatures in order, consecutive repeats folded (`"tool:search x3"`), at most
40 entries. `score` is the number of the run's spans that matched. A failed
run also names the step its failure is attributed to (`cause`), and a run that
declared one carries its `goal`. `outcome=success` or `outcome=failure`
returns only runs that ended that way; the default `any` keeps every run and
ranks successes first. A run that is still active (its last span ended within
the last 15 minutes) is `unknown` and never ranked as a success.

Recall groups the newest 5,000 matching spans into runs and reads the 100 runs
with the most matches among them in full; a query whose matches overflow the
first bound says `"truncated": true`. Each run is read up to its first 20,000
spans; a longer one carries `"truncated": true` of its own, its steps, cost
and skeleton describe those first spans, and its outcome is `unknown` with no
`cause` — so a long run that failed late is never ranked as a success on the
strength of its beginning. Finding the newest matches means ranking
all of them, so words that appear in more than 200,000 spans are refused with
`400` — add a word to narrow them.

## From an agent: the MCP tools

With `--mcp`, three read tools put the same answers in front of a model:
`get_run_tape` (a compact rendering of the tape with input and output
previews), `compare_runs` (the alignment and first divergence, with
structured content), and `recall_runs` (recalled runs and their skeletons).
Stored text in their results stays inside the untrusted-telemetry block, as
everywhere on that surface. There is no replay tool: a replay is for an agent
*harness*, and letting the model under test create one would let stored
telemetry choose which recording its next run is fed. See the
[MCP guide](mcp.md#the-tools).

## Limits, stated plainly

- **Tools execute live** unless your harness takes results from `tools/next`.
- **Streams are reconstructed** from stored text, not the provider's bytes.
- **Only what was recorded can be replayed.** A model step without a recorded
  answer refuses the replay at creation, naming the step. An offloaded answer
  that cannot be read — erased, swept by retention, or past
  `--tape-max-bytes` — refuses it the same way (`409`), and the message says
  which of those it was. A tool step that recorded no result is refused by
  `tools/next` rather than served as `null`. A run longer than 20,000 spans
  is refused rather than cut.
- **Only chat completions and messages are replayed.** The OpenAI Responses
  API, embeddings and legacy completions under the replay's base URL answer
  `400`.
- **Ids the recording lacks are invented.** If a recording's tool calls carry
  no ids, the agent's follow-up request carries Traza's invented id where the
  recording carries none, so strict matching stops at the first tool result;
  use loose mode for such recordings. A model name or response id over 256
  bytes counts as lacking.
- **Answers are decoded by where they were recorded.** An answer recorded as
  text — the `llm.completion` attribute, or an OpenInference `output.value`
  whose `output.mime_type` is not JSON — is served as that text even when it
  reads as JSON (`{"content": "hi", "count": 2}` comes back whole, not as a
  message saying "hi"). A message list (`gen_ai.output.messages`, the indexed
  layouts, native events) is read as messages, and an OpenInference JSON
  `output.value` as the provider's response body.
- **Answer text comes back as it was written.** An answer recorded as text is
  served byte for byte, inline or offloaded — duplicate keys, spacing, key
  order and the spelling of numbers included, even when it reads as JSON. So
  is an OpenInference JSON `output.value` that holds no provider response
  (no `choices`, not a message): it is served as its own text.
- **JSON-shaped tool-call arguments come back compact.** Arguments recorded as
  `{"city": "Paris"}` are served as `{"city":"Paris"}`, keys sorted; the
  normalisation compares them as JSON, so this never causes a divergence.
- **A served response is at most 64 MiB**, streamed or not. A stream repeats
  fixed fields in every chunk and splits tool-call arguments into many, so a
  recording of thousands of tiny tool calls can stream to hundreds of times
  its size; a step that would render past the bound answers `409` with
  `"type": "response_too_large"` and is not consumed (the same step unstreamed
  may fit).
- **Usage is passed through as recorded**, without converting between the
  providers' prompt-caching conventions.
- **An Anthropic answer is one message**, so a recording holding several
  assistant messages is served as their text concatenated and their tool calls
  in order.
- **Concurrent model calls are served in arrival order.** Strict mode reports
  a swapped pair as a divergence rather than serving answers to the wrong
  questions. Calls to one replay take turns, each from reading the next step
  to consuming it, so every served answer is rendered once: a call waits
  while an earlier call to the same replay renders, and calls to other
  replays do not wait. Waiting does not stretch the replay's hour: a call
  whose turn comes after the replay expired is refused `404` without being
  rendered, and an answer that finishes rendering after it is dropped
  unserved, so no step is consumed past `expires_at_ns`.
- **The registry is bounded, per tenant first.** One tenant's data — or the
  operator view, which counts as a share of its own — may be held by at most
  64 live replays and 64 MiB of memory; the registry as a whole holds at most
  1,024 replays and 512 MiB, as the backstop behind every share. The memory
  is counted generously and charged while the snapshot is built: answers,
  tool records, model names, ids, tool names, what erasure is checked
  against, collection overhead, and each replay's call log at its full
  4,096-entry bound (about 96 KiB, so a share holds its 64 replays with room
  to spare). Past a bound, creation answers `429` with `Retry-After`: for a share,
  the time until enough of *that share's* replays expire; for the whole
  registry, the same for unbound operators and a flat 60 seconds for bound
  credentials, which have no business learning when another tenant's replay
  expires. A live replay is never evicted to make room. A snapshot larger than
  a whole share can never be held and is refused with `409`, not told to
  retry — as soon as building it passes the share, not after reading the
  whole run into it.
- **Erasure withdraws replays**, from the moment the erasure is recorded; see
  [above](#erasure-reaches-replays). A withdrawn replay's copy stays in memory
  until its next call or its expiry, and withdrawn replays are bounded like
  live ones.
- **Replays follow tenancy.** A replay created under a tenant can be used only
  by that tenant's credentials and unbound operators; one created from the
  operator view (no tenant) only by unbound operators. A foreign credential
  gets `404`. Replay ids are hard to guess but are not credentials — every
  call still authenticates.
