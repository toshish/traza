@trazadb/sdk — a TypeScript client for TrazaDB's agent-native APIs
==================================================================

A typed client for the parts of [Traza](../../README.md) an agent harness
talks to: the [run fuses](../../docs/guide/guard.md) and the pre-call guard,
[tapes and replays](../../docs/guide/replay.md), diffs, recall, and the
[ledger and evidence packs](../../docs/guide/ledger.md).

It is **not instrumentation**. OpenTelemetry — or OpenLLMetry, or
OpenInference on top of it — records the spans; [`otelEnv`](#sending-traces)
writes its exporter settings for you. This package asks the server about what
was recorded, and acts on the answer.

- ESM, written in TypeScript, shipped as JavaScript with `.d.ts` types.
- No runtime dependencies. The transport is the global `fetch`: Node 20 or
  newer, Deno, Bun and edge runtimes. In a browser the page must be
  same-origin with Traza (behind the reverse proxy that serves your app, say):
  the server sends no CORS headers.
- Every call takes an `AbortSignal`; the event feed is an async iterable.

Install
-------

From this repository:

```sh
cd sdk/typescript
npm ci && npm pack          # builds dist/ and writes trazadb-sdk-<version>.tgz
cd /your/app && npm install /path/to/traza/sdk/typescript/trazadb-sdk-*.tgz
```

Connecting
----------

```ts
import { Client } from "@trazadb/sdk";

const traza = new Client("http://127.0.0.1:8080", { token: process.env.TRAZA_TOKEN, timeoutMs: 10_000 });
const fromEnv = Client.fromEnv(); // TRAZA_URL, TRAZA_TOKEN, TRAZA_TENANT

await traza.whoami(); // { auth: "open", scope: "admin", tenant: null, operator: true, credential: null }
```

`token` is sent as `Authorization: Bearer`; leave it out for an open server.
The client follows no redirect (it asks `fetch` for `redirect: "manual"`), so
the token goes only to the URL you give it: a `3xx` (from a proxy upgrading
the scheme, say) rejects with a `TrazaError` naming the `Location`. `tenant`
scopes every call that takes one, for an unbound credential. A bound
credential can leave it out (the server scopes it to its own tenant) or name
its own tenant; naming any other is refused with `AuthError` (`403`), and
that includes `""`, the default tenant. `fromEnv()` treats a variable set to
the empty string as unset, as the shell hook does, so a stray
`TRAZA_TENANT=` names no tenant. The token is a private field:
`console.log(traza)` and `JSON.stringify(traza)` never show it.

**Timestamps are `bigint`.** Every `*_ns` field is Unix nanoseconds — around
1.8e18, far past what a JavaScript number holds exactly — so the client reads
responses with a lossless parser and every `*_ns` field arrives as a `bigint`
(`tape.steps[0].start_ns - tape.steps[1].start_ns` works;
`Number(verdict.usage.duration_ns) / 1e9` gives seconds). Recorded payloads —
a step's `request` and `response`, a tool step's `arguments` and `result` —
are your application's data and are parsed exactly as `JSON.parse` would.
Response fields keep the wire's snake_case names, so they read the same as
the [HTTP reference](../../docs/guide/http-api.md).

Guarding an agent loop
----------------------

A fuse's verdict on an ingest acknowledgement always arrives at least one
step late — a step's span reaches Traza after the step ran. The pre-call
check is the only surface that can refuse a step **before** it happens, so
that is what the guard asks.

`guard(run, options)` checks and throws `FuseTripped` on `stop`; it resolves
the verdict (`continue` or `warn`) otherwise:

```ts
import { Client, FuseTripped } from "@trazadb/sdk";

const traza = Client.fromEnv();

async function runAgent(runId: string, task: string): Promise<string> {
  let state: State = { runId, messages: [task], done: false };
  try {
    while (!state.done) {
      await traza.guard(runId, { estCostUsd: 0.02 });
      state = await callModel(state);
      for (const call of state.toolCalls ?? []) {
        await traza.guard(runId, { tool: call.name });
        state = await runTool(state, call);
      }
    }
  } catch (error) {
    if (error instanceof FuseTripped) return `stopped by Traza: ${error.reasons.join("; ")}`;
    throw error;
  }
  return state.answer;
}
```

The run id must be the id your instrumentation records the run under — the
span's `session.id` (or `gen_ai.conversation.id`, or OpenLLMetry's
`traceloop.association.properties.session_id`) — so the guard and the
recorded spans talk about the same run.

For a graph whose nodes take the state (LangGraph-style), `guarded` wraps a
node and names the run from its arguments:

```ts
const agentNode = traza.guarded(
  (state: State, config: RunnableConfig) => config.configurable.thread_id,
  async (state: State, config: RunnableConfig) => { /* … */ },
);
const searchNode = traza.guarded((state: State) => state.runId, search, { tool: "search" });
```

A refusal, from a real run where a support agent called `search_orders` with
the same arguments four times under a `max_repeats: 4` stop policy:

```text
FuseTripped: run "sess-7f3a" is stopped: max_repeats 4 reached: tool:search_orders#54985dc3c12fada7 repeated 4 times
```

`check()` asks the same question without throwing — a `stop` there is an
ordinary answer, with `verdict.reasons`, `verdict.usage` and `verdict.limits`.

**If Traza cannot be reached**, the guard throws `TrazaError` (with
`status: null`): running unguarded is a decision, so it is not made for you.
The same goes for an answer the guard cannot act on — a proxy's HTML page, a
body cut short, JSON without a `continue`, `warn` or `stop` verdict — which
is a `TrazaError` too. `{ failOpen: true, onError }` makes the decision for
all of them: the guard resolves `null` and the step runs.

### Operating fuses

```ts
await traza.putPolicy({ name: "support", service: "support-agent", action: "stop",
                        limits: { max_cost_usd: 2.0, max_steps: 40, max_repeats: 4 } });
await traza.policies();                                     // { policies: […], global: {…} }
await traza.runVerdict("sess-7f3a");                        // verdict, usage, limits
await traza.runs({ state: "stop" });                        // stopped runs, most recent first
await traza.trip("sess-9c21", { reason: "customer escalated; take over by hand" });  // the kill switch
await traza.reset("sess-7f3a", { grant: { max_repeats: 8 } });                       // clear a stop
await traza.deletePolicy("fp_58ed88d43fc762ba");
```

### Watching the event feed

```ts
const controller = new AbortController();
const stream = traza.events({ follow: true, signal: controller.signal }); // every read long-polls (25 s)
for await (const event of stream) {
  if (event.kind === "stop" || event.kind === "manual_stop") await pageOncall(event.run, event.reasons);
  await saveCursor(stream.cursor); // resume later with traza.events({ after: cursor })
}
```

A following stream's `waitMs` defaults to 25,000 (`FOLLOW_WAIT_MS`) and may
be anything from 1 to 30,000; `{ follow: true, waitMs: 0 }` throws a
`RangeError`, because it would ask again the instant each empty read came
back. Without `follow`, `waitMs` defaults to 0 and iteration ends at the
first read that returns nothing.
`stream.gap` turns true if the server could no longer honour your cursor —
the feed is a ring of 4,096 events, and a restart empties it. Aborting the
signal ends a long-poll at once with the signal's `AbortError`.

Claude Code
-----------

[`examples/claude-code`](../../examples/claude-code/README.md) is the tested
hook pack (`python3`, standard library only) — the gate, a person's nudges, a
summary of earlier runs at session start, and an installer. The same pre-call
check as a Node `PreToolUse` hook command, saved as an `.mjs` file:

```js
#!/usr/bin/env node
import { Client, TrazaError } from "@trazadb/sdk";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const hook = JSON.parse(input); // Claude Code's hook input
try {
  const verdict = await Client.fromEnv().check(hook.session_id, { tool: hook.tool_name });
  if (verdict.verdict === "stop") {
    console.error(`Traza run fuse: ${verdict.reasons.join("; ")}`);
    process.exit(2); // Claude Code blocks the call and shows stderr to the model
  }
} catch (error) {
  if (!(error instanceof TrazaError)) throw error;
  console.error(`traza: guard unavailable, allowing the call: ${error.message}`); // fail open
}
```

It checks only; the pack's optional `PostToolUse` recorder also records each
call as a span, which gives the session's step and repeat limits something to
count.

**A check delivers the run's nudges.** `check` — and `guard`, which calls it
— is a pre-call check, so its answer also carries the run's pending
[nudges](../../docs/guide/steer.md#nudges) in `verdict.nudges`, once: they
are marked delivered on the server whether or not the caller reads them.
Pass `onNudges` to `guard` to have them handed over before it resolves (or
throws `FuseTripped` for a `block`). Anything that checks a run with this
client consumes its nudges, so give the agent's run id to the agent alone.

Replaying a run with zero model calls
-------------------------------------

`replay(run)` snapshots a recorded run. Point the agent's OpenAI or Anthropic
SDK at it and every model call is answered from the recording — your code,
prompts and control flow run for real; no request goes to a provider.

```ts
import OpenAI from "openai";
import { Client } from "@trazadb/sdk";

const tools: OpenAI.ChatCompletionTool[] = [
  { type: "function", function: { name: "get_weather", parameters: { type: "object", properties: { city: { type: "string" } } } } },
];

const traza = Client.fromEnv();
const replay = await traza.replay("weather-7"); // strict by default
const openai = new OpenAI({ baseURL: replay.openaiBaseUrl, apiKey: traza.token ?? "traza-open" });

const messages: OpenAI.ChatCompletionMessageParam[] = [
  { role: "system", content: "You are a weather assistant." },
  { role: "user", content: "What's the weather in Paris?" },
];
for (;;) {
  const answer = await openai.chat.completions.create({ model: "gpt-4o", messages, tools });
  const message = answer.choices[0]!.message;
  messages.push(message);
  if (!message.tool_calls?.length) break;
  for (const call of message.tool_calls) {
    if (call.type !== "function") continue;
    const recorded = await replay.nextTool(call.function.name); // the recorded result, not a live call
    messages.push({ role: "tool", tool_call_id: call.id, content: JSON.stringify(recorded?.result) });
  }
}
console.log(messages.at(-1)); // … It's raining in Paris, 12°C. Take an umbrella.
console.log(await replay.status()); // served: 2, remaining: 0, diverged: null, …
```

That loop was type-checked and run, as written, against a recorded
OpenLLMetry run with `openai` 6.49.0, and the Anthropic route with
`@anthropic-ai/sdk` 0.129.0
(`new Anthropic({ baseURL: replay.anthropicBaseUrl, apiKey })`).

- `replay.env({ apiKey })` gives `OPENAI_BASE_URL`, `OPENAI_API_KEY`,
  `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` as absolute URLs, for an agent
  process that builds its client from the environment:
  `spawn("node", ["agent.js"], { env: { ...process.env, ...replay.env({ apiKey: agentKey }) } })`.
  The keys are `apiKey`, else your client's Traza token (the Anthropic SDK's
  `x-api-key` is accepted on the replay routes); on an open server a
  placeholder stands in, because the SDKs refuse to start without a key.
- **Hand the agent its own key.** Every library in the agent process can read
  what `env()` exports, and one that reads `OPENAI_API_KEY` but ignores
  `OPENAI_BASE_URL` sends it to the real provider. If your client holds an
  operator token, mint a dedicated `ro` credential bound to the run's tenant,
  create the replay under that tenant (`new Client(url, { token, tenant: "acme" })`),
  and pass the `ro` token as `apiKey` (in the loop above, as the OpenAI
  client's `apiKey`). Any credential of the replay's tenant may consume it.
- **Tools still run live** unless your harness takes their results from
  `nextTool()`, which resolves `null` once no recorded tool step is left.
- In strict mode a request whose messages differ from the recording is refused
  with `409` and `x-should-retry: false`, so the provider SDK throws at once
  rather than retrying; `(await replay.status()).diverged` names the step.
  `{ mode: "loose" }` serves the recorded steps in order regardless.

Comparing runs, and recalling earlier ones
-----------------------------------------

```ts
const diff = await traza.diff("weather-7", "weather-8");
diff.first_divergence?.explanation;
// "step 0 of a and step 0 of b (llm:gpt-4o-2024-08-06) received different input: request 8b9a07d4aac0 in a, 1ae84ec74114 in b"
for (const step of diff.aligned) console.log(step.op, step.why, step.fingerprint_a, step.fingerprint_b);

const { runs } = await traza.recall("weather Paris", { outcome: "any", limit: 5 });
for (const run of runs) console.log(run.run, run.outcome, run.skeleton.join(" -> "));
// weather-7 success llm:gpt-4o-2024-08-06 -> tool:get_weather -> llm:gpt-4o-2024-08-06
```

`tape(run, { resolve })` returns the run's steps with their inputs, outputs,
digests and token counts.

The ledger and evidence packs
-----------------------------

```ts
await traza.ledgerHead(); // { seq: 5, head: "3488…", store_id: "147c…", at_ns: 1792222899559863000n }
const pack = await traza.evidence({ since: startNs, until: endNs });
await pack.verifyDigest(); // true: recomputed from the bytes the server sent

import { verifyEvidenceDigest } from "@trazadb/sdk";
await verifyEvidenceDigest(await readFile("pack.json", "utf8")); // a pack saved earlier
```

The digest is the SHA-256 of the canonical JSON of every field but `digest`,
with every number kept exactly as the server printed it — re-printing them
through JavaScript numbers would change the bytes (`2.0` becomes `2`). It
uses `crypto.subtle`, which a browser offers only on secure (https or
localhost) pages.

Sending traces
--------------

```ts
import { otelEnv } from "@trazadb/sdk";

Object.assign(process.env, otelEnv("http://127.0.0.1:8080", { token: "trz_…", tenant: "acme" }));
// then start the OpenTelemetry SDK as usual
```

```js
{
  OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "http://127.0.0.1:8080/v1/traces",
  OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: "http/protobuf",
  OTEL_EXPORTER_OTLP_TRACES_HEADERS: "Authorization=Bearer%20trz_…",
  OTEL_RESOURCE_ATTRIBUTES: "traza.tenant=acme",
}
```

These are the trace-signal variables, so an SDK with metrics or logs
exporters on does not send those to Traza, which ingests traces only.
`OTEL_RESOURCE_ATTRIBUTES` replaces any value you already export; merge it by
hand if you set others.

Errors
------

| Class | When |
|---|---|
| `TrazaError` | Any refusal, and `status: null` when no response arrived (unreachable, timed out). Also a redirect (its `3xx` status; none is followed) and an answer that cannot be read — not JSON, not the object the route answers, cut short — with the text that arrived on `body`. `status`, `message`, `type` (e.g. `replay_divergence`), `body`, `retryable` |
| `AuthError` | `401`, or `403` with `required` naming the missing grant (`rw`, `admin`, `unbound`) |
| `NotFound` | `404` — another tenant's object included |
| `Conflict` | `409` — a run that cannot be replayed, a strict divergence, `--no-fuses` |
| `Gone` | `410` — a replay withdrawn by an erasure, or exhausted |
| `QuotaExceeded` | `429`, with `retryAfter` in seconds and `quota` for a quota or pause (`null` when a replay was refused for want of room) |
| `FuseTripped` | Thrown by `guard` on `stop`. **Not** a `TrazaError`: a stop is an answer, not a failed request |

Your own `AbortSignal` cancelling a call rejects with the signal's reason (an
`AbortError`), untouched. Routes this client does not wrap are reachable
through `traza.request(method, path, { query, body, signal })`, with the same
credential and error mapping; a `bigint` in a body is written as an exact JSON
integer.

Testing
-------

```sh
cargo build --release   # or point TRAZA_SERVER_BIN at a built traza-server
cd sdk/typescript && npm ci && npm test
```

`npm test` compiles with `tsc` and runs `node --test` against the compiled
package. The suite starts two real servers on ephemeral ports — one open,
one with `TRAZA_TOKENS` — and runs every method against their responses,
including a fuse stop throwing `FuseTripped`, a replay consumed over the
OpenAI and Anthropic routes with plain `fetch`, an aborted long-poll, a quota
refusal, and an evidence pack's digest. Redirects and unreadable answers,
which the real server never sends, come from a canned-answer stub
(`test/stub.js`), and `test/server.test.js` checks that the harness itself
fails fast — no hung process, no server or directory left behind — when the
server never comes up. It takes a second or two.
