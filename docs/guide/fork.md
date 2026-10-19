# Fork & Rewind, and tape tests

A [replay](replay.md) re-runs an agent against its own recording. This guide
covers what you can do once a replay can be told where the recording stops
being the truth:

- **fork** a run at any step — serve a different model answer or tool result
  there, and let the agent carry on, for real, down a branch the recording
  never took;
- **rewind** a long run — replay its beginning for free, then let the agent
  continue live from any step;
- **find a run's forks again**, each compared with its parent: where it
  parted, how it ended, what it cost;
- turn recorded runs into **tape tests** — a strict replay the agent must
  walk step for step, with a failure that names the message, field and
  character where it left the path — from pytest, vitest or jest, or for
  every **golden run** at once with `traza test`, which writes a JUnit report.

Traza never calls a model. A fork's live half is the agent's own traffic,
from the agent's own process: the server's part is to stop serving the
recording at the cut and say so. Route shapes are in the
[HTTP API reference](http-api.md#runs-tapes-replays-diffs-and-recall).

## Thirty seconds of it

The examples use the research run from the [replay guide](replay.md):
`dr-0917`, eighteen steps in which a coordinator hands a web search to a
sub-agent, then fetches the sources it found and writes a brief. Step 10 is
its first fetch of the cost page. Ask: what would the coordinator have
written had that page been gone?

```sh
curl -s -X POST localhost:8080/v1/replays -d '{
  "run": "dr-0917",
  "edits": [{"step": 10, "tool_result": {"error": "fetch failed: 404"}}],
  "live_after": 10
}'
```

```json
{
  "id": "rp_a63207a11b0a3eaad73d832eb2e01dec",
  "run": "dr-0917",
  "mode": "strict",
  "steps": 18,
  "llm_steps": 5,
  "expires_at_ns": 1792390270730002000,
  "unreadable_tool_steps": [],
  "base_urls": {
    "openai": "/v1/replays/rp_a63207a11b0a3eaad73d832eb2e01dec/openai/v1",
    "anthropic": "/v1/replays/rp_a63207a11b0a3eaad73d832eb2e01dec/anthropic"
  },
  "fork_id": "fk_7aa99871913c76bfa3336deba92d857d",
  "fork": {
    "id": "fk_7aa99871913c76bfa3336deba92d857d",
    "parent": "dr-0917",
    "step": 10,
    "edits": [{"step": 10, "kind": "tool_result"}],
    "live_after": 10,
    "attributes": {
      "traza.fork.parent": "dr-0917",
      "traza.fork.step": 10,
      "traza.fork.id": "fk_7aa99871913c76bfa3336deba92d857d"
    }
  }
}
```

The agent's first five model calls — the coordinator's three and the
sub-agent's two — are answered from the recording, exactly as a plain replay
would. Its harness asks `tools/next` for the result of each tool it runs
itself — the search and the fetches; the `Agent` tool runs the sub-agent,
whose model calls are on the tape. The search and the first fetch come from
the recording; the fetch at step 10 gets the edit, marked as one:

```json
{"step": 10, "tool": "WebFetch",
 "arguments": {"prompt": "List the 2026 figures on pilot lines, yields, cost per kWh and sample cells.",
               "url": "https://www.example-batteries.com/reports/ssb-cost-2026"},
 "result": {"error": "fetch failed: 404"}, "edited": true}
```

The agent sends the failed fetch back with its next model call — step 11,
past the live point — and the replay hands the run over:

```text
HTTP/1.1 409 Conflict
x-should-retry: false

{"error":{"type":"replay_live","live_after":10,"served":5,
  "fork_id":"fk_7aa99871913c76bfa3336deba92d857d",
  "message":"this fork goes live after step 10: the recording is not served past it, so send this call to the real provider (or run the real tool) — the run continues from here as a new branch"},
 "type":"error"}
```

That `409` is the harness's signal to send this call, and every later one, to
the real provider. The [Python and TypeScript clients](#from-python-trazafork)
do it for you: the agent never sees the `409`, only a provider that answers.

## Forking a run

`POST /v1/replays` takes two more fields:

| Field | Meaning |
|---|---|
| `edits` | Up to 64 of `{"step": n, "response": …}` (at an llm step) or `{"step": n, "tool_result": …}` (at a tool step), at most one per step |
| `live_after` | A tape step. After it, every model call and every `tools/next` answers `409 replay_live` |

A replay with either is a **fork**: its creation response adds `fork_id` and
`fork` (above), and its status adds the same, with which edits have been
served and when the live point was first reached.

**Strict matching applies up to and including the first edited step, and is
off after it.** At the edited step the agent still asks the recorded question
— it has not seen the edit yet — so a different question there is a
divergence like any other. After it, the agent has seen something the
recording never did, and its requests cannot be expected to match: the
recorded answers that follow (when there is no live point) are served in
order, and each call's `matched` is recorded for you to read. A fork with a
live point and no edit is strict all the way to the live point.

**The cut** — `fork.step`, and the `traza.fork.step` attribute — is the first
step whose answer is not the parent's: the first edited step, or the step
after the live point, whichever comes first.

**An edited answer** is answer text, one assistant message in any shape a
recording holds (OpenAI's `{"content", "tool_calls"}`, Anthropic's content
blocks, the normalised `{"role", "content", "tool_calls"}`), a list of them,
or an OpenAI response body with `choices`. It is served in the asked-for
provider's shape, streamed or not, like a recorded answer — with the
recording's model name, an invented response id (the recorded one named a
different answer) and zero usage (no provider produced it). One that holds
neither text nor a tool call is refused. An edited **tool result** is any
JSON, `null` included, served as the step's `result` with `"edited": true`;
the arguments stay the recording's. An edit can stand in for something the
recording lacks: an edited step's recorded answer is never read, so it need
not have been captured.

**Nothing past the live point is held.** The snapshot stops there, so those
steps need not be replayable at all — a long run whose later answers were
offloaded past `--tape-max-bytes`, or recorded with content capture off, can
still be rewound to any point before them. `llm_steps` counts what is held.

**Rewind** is a live point alone: `{"run": "dr-0917", "live_after": 11}`
replays steps 0 to 11 exactly, then everything after is live. That is how the
harness that refuses to fetch a URL twice was tried on this run. Step 12 is
the repeated fetch it refuses, so the recording is served up to the call that
asked for it, and the rest ran live. Re-running a long agent from step 40
costs nothing for its first 40 steps.

**A run longer than a replay holds can still be rewound.** A plain replay
refuses a run past 20,000 spans rather than replaying its beginning as if the
recording ended there. A fork whose live point falls within the steps those
first 20,000 spans hold serves nothing past that point, so it is accepted:
its creation response says `"steps_truncated": true`, and `steps` counts the
steps read, not the run's. A live point past them is refused for the run's
length.

Refusals are `400` and say what to change:

```json
{"error":"step 10 is a tool call (tool:WebFetch), so a \"response\" edit cannot replace it: a tool result is edited with \"tool_result\""}
```

— and likewise an edit or live point past the run's last step, an edit after
the live point (it would never be served), two edits of one step, an unknown
field, or an edited answer that is not an answer.

### Tag the forked run

A fork's harness gives the forked run **its own session id** and stamps every
span it records with the creation response's `fork.attributes`:
`traza.fork.parent`, `traza.fork.step` and `traza.fork.id`. The clients do
both (below). Without its own session id the fork's spans would carry the
parent's — the agent is the same code with the same input — and join the
parent run instead of forming a new one; the fork listing counts such spans
as `merged_into_parent` rather than hiding them.

## Finding a run's forks

`GET /v1/forks?run=dr-0917` lists every run whose spans carry
`traza.fork.parent = dr-0917`, the most recently started first, each read
and diffed against its parent in its own tenant. Here there is one: the
[rewind](#forking-a-run) under the deduping harness, run with
`traza.fork("dr-0917", at_step=12, run_id="dr-0917-dedupe")` and recorded
under that session id. The 404 fork above is not listed: nothing ran past its
live point, so it recorded nothing.

```json
{
  "run": "dr-0917",
  "parent": {"run": "dr-0917", "steps": 18, "cost_usd": 0.27121787500000005, "tokens": 3341,
             "duration_ns": 58335000000, "errors": 0, "outcome": "success", "truncated": false},
  "forks": [{
    "run": "dr-0917-dedupe",
    "tenant": "",
    "fork_id": "fk_a9277a1d7df0130387db2879c908035b",
    "fork_step": 12,
    "first_start_ns": 1792381327733000000,
    "last_end_ns": 1792381378990000000,
    "tagged_spans": 16,
    "diffed": true,
    "steps": 16,
    "outcome": "success",
    "cost_usd": 0.202954675,
    "errors": 0,
    "first_divergence": {"a": 12, "b": 12, "kind": "decision",
      "explanation": "step 12 of a and step 12 of b (tool:WebFetch) received the same input and produced a different output: response e450b5a73db1 in a, 750af04196bb in b"},
    "delta": {"cost_usd": -0.06826320000000005, "tokens": -105, "duration_ns": -7078000000,
              "errors": 0, "steps": -2},
    "diverged_before_cut": false,
    "truncated": false
  }],
  "merged_into_parent": 0,
  "spans_examined": 16,
  "truncated": false
}
```

The fork parted at its cut, step 12, and nowhere before it. It ended well,
two steps shorter and about seven cents cheaper.

`first_divergence` is the [differ's](replay.md#comparing-two-runs), with the
parent as `a`. `diverged_before_cut` says whether the fork parted from its
parent *before* the step it was cut at — the replayed beginning did not
re-execute as recorded, which usually means a tool ran live and answered
differently. A fork whose parent has no spans in its tenant is listed with
`"diffed": false, "skipped": "parent_missing"`.

The tagged-span read, the parent's read, and every fork's read and diff
spend one [`--query-deadline-ms`](../configuration.md#server-flags) budget.
A fork the budget did not reach is listed undiffed (`"skipped": "deadline"`)
and the listing says `"truncated": true` — a partial listing is more use than
a refusal; the tagged-span read and the parent's read themselves are refused
`400` past the budget, as everywhere. `limit` (default 20, at most 50) bounds
the forks listed; at most 10,000 tagged spans are examined. Those are the
**earliest-starting** 10,000 (a sorted read would have to hold every match),
so past that cap the oldest forks are listed and the newest may be missing —
`truncated` says so.

## From Python: `traza.fork`

```python
import traza

with traza.fork("dr-0917", at_step=10, tool_result={"error": "fetch failed: 404"}) as fork:
    brief = run_research(TASK, recorded=fork.tool_result)   # steps 0-9 replayed, 10 edited, 11 on live
print(fork.id, fork.final_status.fork.live_calls)
print(fork.diff().first_divergence.explanation)
```

`at_step` is the cut. `response=` edits a model answer, `tool_result=` a tool
result; with neither, the recording simply stops before `at_step`.
`live=False` serves the edit and lets the recording play on.

The harness takes its tool results from the callback it is handed, as in a
[tape test](#python-and-pytest) — here `fork.tool_result`. Before it runs a
tool of its own (the search, a fetch) it asks the callback for that tool's
result. It uses the step's `result` when one comes back, and runs the real
tool on `None`. The `Agent` tool is not one of those: it runs the sub-agent,
whose model calls are on the tape. So the fetch at step 10 gets the 404, and
the fetches after the cut run for real.
`fork.call_tool(name, fn, *args)` makes that choice in one call. The switch
below carries model calls only, so an edited tool result reaches the agent
through its harness or not at all.

Inside the block, `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` point at a
**switch**: a small HTTP server on a loopback port, private to the process,
that forwards each model call to the replay until the replay answers
`replay_live`, and from then on to the real provider — the base URL and key
the environment named when the fork was made (`OPENAI_BASE_URL`, else
OpenAI's API, and `OPENAI_API_KEY`; the same for Anthropic), as it stood
before any fork changed it, or the `live_anthropic_base_url` /
`live_anthropic_api_key` options (and their OpenAI twins). The agent sees one
provider throughout. The switch listens on `127.0.0.1` only and answers only
requests carrying the per-fork secret it put in `OPENAI_API_KEY` /
`ANTHROPIC_API_KEY`, so nothing else on the machine can borrow the real key
it forwards with — and inside the block those variables hold the secret, not
the real key, so the agent's SDK never sends it anywhere. A provider client
built *before* the block keeps the URL it was built with — build it inside,
or pass `fork.anthropic_kwargs()` (`fork.openai_kwargs()` for the OpenAI
SDK).

A `None` from `fork.tool_result(name)` says nothing about model calls: the
server keeps a tool cursor and a model cursor apart, and answers
`replay_live` for a tool the recording does not hold — a setup tool the
agent calls before its first model call, say — while recorded model calls
are still to come. Only the model endpoint's own `replay_live` moves the
switch (and `fork.is_live`), so those calls are still answered from the
recording, strictly matched.

**The environment is one fork's at a time.** Starting a second fork that
would set it while one's block is active raises `RuntimeError` before
anything is created. Forks that overlap — nested, or one per thread for
parallel what-ifs — take `set_env=False` and give each agent its own
`fork.anthropic_kwargs()` (or `fork.env()` for a subprocess); each still
finds the real provider, never another fork's switch:

```python
from concurrent.futures import ThreadPoolExecutor
from anthropic import Anthropic

def what_if(failure):
    with traza.fork("dr-0917", 10, tool_result={"error": failure}, set_env=False) as fork:
        llm = Anthropic(**fork.anthropic_kwargs())
        return run_research(TASK, client=llm, recorded=fork.tool_result)

briefs = list(ThreadPoolExecutor(4).map(what_if, ["fetch failed: 404", "fetch failed: 403",
                                                  "timed out", "paywalled"]))
```

**Spans are tagged** when the OpenTelemetry SDK is configured before the
fork is entered: a span processor is added to the global tracer provider
once, and stamps the fork's attributes and its session id (`fork.run_id`,
the fork id unless you name one) on every span that starts in the block's
context. A span another thread starts meanwhile — a web server's other
request — keeps its own run: Python threads do not inherit context
variables, so a worker thread of the agent's is tagged only when it runs in
a copy of the block's context (`contextvars.copy_context().run`), or when
the fork is made with `tag_threads=True` — which tags every thread's spans,
for a process that runs nothing but the forked agent, and is allowed for one
fork at a time. Without the SDK, `fork.span_attributes()` is the dict to
stamp yourself, and `fork.env()` carries `TRAZA_FORK_ID`,
`TRAZA_FORK_PARENT`, `TRAZA_FORK_STEP`, `TRAZA_FORK_RUN` and
`TRAZA_REPLAY_ID` for a subprocess agent along with the switch's URLs and
key. From a shell, `traza fork` runs the agent with that environment and
`TRAZA_URL`; its harness asks
`POST $TRAZA_URL/v1/replays/$TRAZA_REPLAY_ID/tools/next` for the result of
each tool it runs itself, and runs the real tool when that answers
`409 replay_live`:

```sh
traza fork dr-0917 --at 10 --tool-result '{"error": "fetch failed: 404"}' -- python research.py
```

## From TypeScript: `client.fork`

```ts
const fork = await traza.fork("dr-0917", { atStep: 10, toolResult: { error: "fetch failed: 404" } });
const anthropic = new Anthropic(fork.anthropicOptions());   // { baseURL, apiKey, fetch }
const recorded = (name: string) => fork.toolResult(name);   // the edit at step 10; null past the cut
await runResearch(anthropic, { recorded });
fork.close();
```

The switch is the `fetch` the fork hands the SDK — both the OpenAI and the
Anthropic SDKs take one — so no port is opened: it sends each call to the
replay until it answers `replay_live`, then that call and every later one to
the real provider (`liveOpenAI` / `liveAnthropic` options, else the
environment's base URL and key). Each fork is its own `fetch`, so any number
can run side by side in one process. As in Python, the harness takes tool
results from `fork.toolResult(name)` and runs the real tool on `null`
(`fork.callTool(name, fn)` makes that choice in one call). A `null` — past
the cut, or a tool the recording lacks — moves no model call off the
recording; only a model call answered `replay_live` does (`fork.isLive`).

`fork.spanProcessor()` returns a duck-typed OpenTelemetry span processor
that stamps the fork's attributes and session id on the spans started **in
the fork's OpenTelemetry context**, until `fork.close()`. Run the agent
there, and a server handling other requests meanwhile keeps their spans in
their own runs:

```ts
import { context } from "@opentelemetry/api";

provider.addSpanProcessor(fork.spanProcessor());
await fork.run(context, () => runResearch(anthropic, { recorded }));   // context.with(fork.context(context.active()), …)
```

The fork is carried under `Symbol.for("traza.fork")` (exported as
`FORK_CONTEXT_KEY`), which is what `createContextKey("traza.fork")` returns.
`fork.spanProcessor({ allSpans: true })` tags every span while the fork is
open, whatever its context — only for a process that runs nothing else.

## Tape tests

A recorded run is a regression test waiting to happen. Replay it strictly and
the agent either walks the same path or says where it left it — with zero
model calls. When a strict replay refuses a request, the refusal now carries
**where the request first differs** from the recording. Here the harness that
refuses to fetch a URL twice is replayed strictly against `dr-0917`, and its
first model call after the refused fetch is refused:

```json
{"error":{"type":"replay_divergence","step":13,
  "difference":{"message":9,"role":"user","field":"content","at_char":13,
                "recorded":"[{\"content\":\"Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields…",
                "got":"[{\"content\":\"Already fetched https://www.example-energy.org/2026/solid-state-pilot-lines earlier in this run; reusing th…",
                "recorded_messages":10,"got_messages":10},
  "expected_digest":"38d64ab342a7ff12be1b446cc0209eecabbae76f5d4ec73b59a9e88a178ddd6d",
  "got_digest":"2d02f0080506df8f483cde7735ec2240ee801fdbd82f82836201aa8a882f0cb1",
  "message":"this request's messages do not match recorded step 13: …"},
 "type":"error"}
```

A replay holds only each recorded request's digest, so the recorded request
is read again from the step's own span, under the span's tenant and the query
budget, and compared message by message under the same normalisation strict
matching uses. `field` is the first of `role`, `content`, `tool_call_id`,
`tool_calls` that differs — or `missing` / `extra` when one side has fewer
messages — and each side is quoted (120 characters) from just before
`at_char`; `role` is cut to 32 characters, since the request chooses it. The
first divergence keeps its difference in the replay's status, and that same
request refused again is answered from it without a read. When the recorded
request cannot be re-read (erased, offloaded and gone, not a message list,
or past the budget) there is no `difference`, and the digests are all there
is.

**Each replay re-reads its recording at most 8 times** to explain
divergences. A harness whose every retry differs — a timestamp or a fresh id
in its prompt — would otherwise turn a route that reads nothing into a store
read per call. Past the eighth, a divergence is refused as usual, without a
`difference`; the first divergence's, already kept, is still reported.

The status also counts `exhausted_calls`: model calls refused because every
recorded step had been served — an agent that asked for more than the
recording holds.

### Python and pytest

```python
from traza import Client
from traza.testing import replay_test

def test_research_agent_follows_its_recording():
    replay_test(Client.from_env(), "dr-0917", lambda tape: run_research(TASK, recorded=tape.tool_result))
```

`replay_test` points `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` at a strict
replay while the agent runs, then raises `TapeMismatch` — an
`AssertionError` — when the agent diverged, stopped early, or overran. The
agent takes its tool results from the recording too (`tape.tool_result`), so
the pages it is shown are the recorded ones. Under the deduping harness the
test fails so:

```text
the agent diverged from recorded run dr-0917 at step 13:
  message 9 (user): content differs at character 13
    recorded: '[{"content":"Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields…'
    got:      '[{"content":"Already fetched https://www.example-energy.org/2026/solid-state-pilot-lines earlier in this run; reusing th…'
  (the recording's request has 10 messages, the agent's 10)
```

The package also registers **pytest-traza** (a `pytest11` entry point; with
the source tree alone, `-p traza.pytest_plugin`):

```python
import pytest

@pytest.mark.traza_tape("dr-0917")
def test_follows_the_recording(traza_tape):
    run_research(TASK, recorded=traza_tape.tool_result)   # every model call answered from the run
```

A marked test runs against a strict replay of the run and fails with the
message above when the agent leaves the path; `mode="loose"` serves in order
regardless. The `traza_tape` fixture is the `TapeTest` (recorded tool results
via `tool_result()`, the environment for a subprocess agent). The server is
`--traza-url` or `TRAZA_URL`, the tenant `--traza-tenant` or
`TRAZA_TENANT`, the credential `TRAZA_TOKEN` — never a command-line option,
where it would land in process listings and CI logs. The agent's provider
key is [`TRAZA_REPLAY_KEY`](#the-agents-key) when set. A marked test that
skips itself, or is `xfail` and fails as expected, is reported as it was:
the tape is judged only for a test that ran to an end. The plugin needs
pytest; the package itself still depends on nothing.

### TypeScript: vitest, jest, node:test

```ts
import { Client, replayTest } from "@trazadb/sdk";

test("the research agent follows its recording", async () => {
  await replayTest(new Client(url), "dr-0917", async (replay) => {
    await runResearch(new Anthropic({ baseURL: replay.anthropicBaseUrl, apiKey: "traza" }), {
      recorded: (name) => replay.nextTool(name),
    });
  });
});
```

`replayTest` throws a `TapeMismatch` with the same message, which every test
runner reports as a failure; an error the agent threw because it was refused
is its `cause`.

## Golden runs and `traza test`

A **golden run** is a run marked as a regression baseline, one of two ways:

- **recorded so** — any of its spans carries `traza.golden = true` (the
  boolean, or exactly the string `"true"`);
- **marked after the fact** — an annotation named `traza.golden` on its
  session (`session_id`), or on its trace with no span (which names the run
  `trace:<trace_id>`). The newest such annotation decides, so a later
  `false` un-marks a run whatever its spans say. `traza golden add RUN` /
  `remove RUN` and `client.mark_golden(run)` / `markGolden(run)` write it
  (an `rw` credential).

Both read a value by one rule: the boolean, or exactly the string `"true"`
or `"false"`. `"True"` — Python's `str(True)` — marks nothing either way.

`GET /v1/goldens` lists them, ordered by tenant then run so a report reads
the same every time:

```json
{"runs":[{"run":"dr-0917","tenant":"","source":"annotation","tagged_at_ns":1792386745895029000}],
 "spans_examined":0,"annotations_examined":1,"truncated":false}
```

`traza test` replays every golden run strictly under your agent command and
writes a JUnit XML report (stdout, or `--junit FILE`):

```sh
traza test -- python agent.py --task "{input}"
```

For each run it creates a strict replay and runs the command with the
replay's environment — `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL` and their
keys, `TRAZA_RUN`, `TRAZA_REPLAY_ID`, and `TRAZA_RUN_INPUT`, the text of the
last user message of the run's first model call (for most agents, the task
they were given). `{run}`, `{input}` and `{replay}` in the command's
arguments are replaced by the same values. A run passes when the command
exits 0 having made exactly the recorded model calls; it fails as
`divergence` (with the difference), `incomplete`, `overrun`, `exit` or
`timeout` (`--timeout`, 300 s by default), and is an `error` when its replay
cannot be created. Each `testcase` is named by run id, classed
`traza.golden` (`traza.golden.<tenant>` for a named tenant), with the tail of
the agent's output (64 KiB of each stream) as `system-out` / `system-err`.
Both streams are read as the agent writes them, so that tail is all the
runner holds — an agent that prints gigabytes before it finishes, or until
its timeout, costs the runner 64 KiB a stream, not its output.

The exit status is 0 when every run passed and 1 otherwise. **An empty suite
is a failure** unless `--allow-empty` is given: a CI step that tests nothing
should not pass. **So is a partial one**: `traza test` lists up to 1,000
golden runs (`--limit`, the server's maximum by default), and when the
listing says it is incomplete — more runs than the limit, or more golden
markers than one listing examines — the runs listed are tested and the suite
fails anyway, saying why, unless `--allow-partial` is given. Past 1,000
golden runs, split the suite with `--tenant` or `--run`. `--run RUN`
(repeatable) tests named runs instead of the golden ones. Exit status 2 is a
usage error or a server that could not be asked.

### The agent's key

The agent under test sends a key as `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`,
and any library that reads that variable but ignores `OPENAI_BASE_URL` sends
it to the real provider. So `traza test` and pytest-traza hand the agent
**`TRAZA_REPLAY_KEY`** when it is set — a dedicated `ro` credential bound to
the runs' tenant, which any replay of that tenant accepts — and otherwise
`TRAZA_TOKEN`, **refusing** (exit 2, or a pytest usage error) when that is an
unbound operator credential, which can read every tenant. To hand it over
anyway, say so: `TRAZA_REPLAY_KEY=$TRAZA_TOKEN`. One replay key serves one
tenant; a suite spanning tenants runs once per tenant (`--tenant`).

## From an agent: `list_forks`

With `--mcp`, the read tool `list_forks` puts the fork listing in front of a
model — each fork's cut, outcome, cost and where it parted — inside the
untrusted-telemetry block like every stored string on that surface. There is
deliberately no tool that creates a fork, for the same reason there is none
that creates a replay ([MCP guide](mcp.md#the-tools)).

## Limits, stated plainly

- **Traza does not go live; your harness does.** The server stops serving at
  the live point and answers `409 replay_live`. The clients switch to the
  real provider; a harness of your own must do the same on that answer —
  and must keep tools and model calls apart, as the clients do: a
  `tools/next` `replay_live` means run that tool for real, not that model
  calls may skip the recording.
- **After an edit without a live point, the recording plays on loosely**,
  answering a conversation it was not recorded in. That exercises the agent's
  own code down the recorded path; it is not what a model would have said.
- **An edited answer has an invented response id and zero usage**, and is
  served under the recording's model name.
- **A fork is a replay**: it lives an hour, in memory, counts against the
  same [registry bounds](replay.md#limits-stated-plainly), and an erasure of
  what it holds withdraws it. Its edits are your data, held as long as it is.
- **The divergence explanation is best effort**: it re-reads the recorded
  span, at most 8 times per replay, and says nothing when it cannot.
- **Golden markers are session or trace annotations, or span attributes.** An
  annotation on a single span marks nothing.
- **Bounds:** 64 edits per fork; a fork listing examines 10,000 tagged spans
  (the earliest-starting: past the cap the newest forks can be missing) and
  lists at most 50 forks, each diffed within the differ's own caps (5,000
  steps, 50,000 spans a side); a golden listing examines 20,000 tagged spans,
  decides from the newest 10,000 golden annotations — walking every one in
  scope under the query budget, never holding more than 10,000 — and lists
  at most 1,000 runs (200 by default); a divergence's kept difference quotes
  120 characters a side and 32 of a role.
- **A run past 20,000 spans replays only as a rewind** whose live point falls
  within the steps read; a plain replay, or a later live point, is refused.
- **The Python switch acts on the process environment**, so one fork at a
  time may set it; overlapping forks take `set_env=False`. The variables are
  restored when the block ends; a provider client built before it is not
  redirected.
- **Span tagging follows context.** A span started outside the fork's
  context — another thread in Python (unless `tag_threads=True`), another
  async context in Node (unless `allSpans: true`) — is not tagged, and an
  instrumentation that sets its own `session.id` after a span starts is not
  re-tagged.
- **pytest-traza loads wherever the package is installed.** For an unmarked
  test it does nothing.
