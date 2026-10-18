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

The examples use the weather run from the [replay guide](replay.md):
`weather-7`, a model call that asks for `get_weather` (step 0), the tool
(step 1), and the model call that answers (step 2). Ask: what would the agent
have said had the tool returned sunshine?

```sh
curl -s -X POST localhost:8080/v1/replays -d '{
  "run": "weather-7",
  "edits": [{"step": 1, "tool_result": {"forecast": "sun", "celsius": 24}}],
  "live_after": 1
}'
```

```json
{
  "id": "rp_c28b383ca80306a558138f514ad39934",
  "run": "weather-7",
  "mode": "strict",
  "steps": 3,
  "llm_steps": 1,
  "expires_at_ns": 1792302016223771000,
  "unreadable_tool_steps": [],
  "base_urls": {
    "openai": "/v1/replays/rp_c28b383ca80306a558138f514ad39934/openai/v1",
    "anthropic": "/v1/replays/rp_c28b383ca80306a558138f514ad39934/anthropic"
  },
  "fork_id": "fk_b6956c347b2ab03ff5b1fc530c36754d",
  "fork": {
    "id": "fk_b6956c347b2ab03ff5b1fc530c36754d",
    "parent": "weather-7",
    "step": 1,
    "edits": [{"step": 1, "kind": "tool_result"}],
    "live_after": 1,
    "attributes": {
      "traza.fork.parent": "weather-7",
      "traza.fork.step": 1,
      "traza.fork.id": "fk_b6956c347b2ab03ff5b1fc530c36754d"
    }
  }
}
```

The agent's first model call is answered from the recording, exactly as a
plain replay would. Its harness asks `tools/next` for the tool result and
gets the edit, marked as one:

```json
{"step": 1, "tool": "get_weather", "arguments": {"city": "Paris"},
 "result": {"celsius": 24, "forecast": "sun"}, "edited": true}
```

The agent sends the sunny result back with its next model call — step 2, past
the live point — and the replay hands the run over:

```text
HTTP/1.1 409 Conflict
x-should-retry: false

{"error":{"type":"replay_live","live_after":1,"served":1,
  "fork_id":"fk_b6956c347b2ab03ff5b1fc530c36754d",
  "message":"this fork goes live after step 1: the recording is not served past it, so send this call to the real provider (or run the real tool) — the run continues from here as a new branch"}}
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

**Rewind** is a live point alone: `{"run": "weather-7", "live_after": 1}`
replays steps 0 and 1 exactly, then everything after is live. Re-running a
long agent from step 40 costs nothing for its first 40 steps.

Refusals are `400` and say what to change:

```json
{"error":"step 1 is a tool call (tool:get_weather), so a \"response\" edit cannot replace it: a tool result is edited with \"tool_result\""}
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

`GET /v1/forks?run=weather-7` lists every run whose spans carry
`traza.fork.parent = weather-7`, the most recently started first, each read
and diffed against its parent in its own tenant:

```json
{
  "run": "weather-7",
  "parent": {"run": "weather-7", "steps": 3, "cost_usd": 0.0, "tokens": 233,
             "duration_ns": 1000000000, "errors": 0, "outcome": "success", "truncated": false},
  "forks": [{
    "run": "fk_b6956c347b2ab03ff5b1fc530c36754d",
    "tenant": "",
    "fork_id": "fk_b6956c347b2ab03ff5b1fc530c36754d",
    "fork_step": 1,
    "first_start_ns": 1791964860000000000,
    "last_end_ns": 1791964861000000000,
    "tagged_spans": 4,
    "diffed": true,
    "steps": 3,
    "outcome": "success",
    "cost_usd": 0.0,
    "errors": 0,
    "first_divergence": {"a": 1, "b": 1, "kind": "decision",
      "explanation": "step 1 of a and step 1 of b (tool:get_weather) received the same input and produced a different output: response 25ac26944464 in a, 05b04b8ed40a in b"},
    "delta": {"cost_usd": 0.0, "tokens": 0, "duration_ns": 0, "errors": 0, "steps": 0},
    "diverged_before_cut": false,
    "truncated": false
  }],
  "merged_into_parent": 0,
  "spans_examined": 4,
  "truncated": false
}
```

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
the forks listed; at most 10,000 tagged spans are examined.

## From Python: `traza.fork`

```python
import traza

with traza.fork("weather-7", at_step=1, tool_result={"forecast": "sun", "celsius": 24}) as fork:
    answer = run_agent("What's the weather in Paris?")   # 0 replayed, 1 edited, 2 live
print(fork.id, fork.final_status.fork.live_calls)
print(fork.diff().first_divergence.explanation)
```

`at_step` is the cut. `response=` edits a model answer, `tool_result=` a tool
result; with neither, the recording simply stops before `at_step`.
`live=False` serves the edit and lets the recording play on.

Inside the block, `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` point at a
**switch**: a small HTTP server on a loopback port, private to the process,
that forwards each model call to the replay until the replay answers
`replay_live`, and from then on to the real provider — the base URL and key
the environment named when the fork was made (`OPENAI_BASE_URL`, else
OpenAI's API, and `OPENAI_API_KEY`; the same for Anthropic), or the
`live_openai_base_url` / `live_openai_api_key` options. The agent sees one
provider throughout. The switch listens on `127.0.0.1` only and answers only
requests carrying the per-fork secret it put in `OPENAI_API_KEY` /
`ANTHROPIC_API_KEY`, so nothing else on the machine can borrow the real key
it forwards with — and inside the block those variables hold the secret, not
the real key, so the agent's SDK never sends it anywhere. A provider client
built *before* the block keeps the URL it was built with — build it inside,
or pass `fork.openai_kwargs()`.

`fork.tool_result(name)` returns the next recorded tool step — the edit, at
the edited step — or `None` once the harness should run the real tool;
`fork.call_tool(name, fn, *args)` does that choice for you.

**Spans are tagged** when the OpenTelemetry SDK is configured before the
fork is entered: a span processor is added to the global tracer provider
once, and stamps the active fork's attributes and its session id
(`fork.run_id`, the fork id unless you name one) on every span that starts
inside the block. Without the SDK, `fork.span_attributes()` is the dict to
stamp yourself, and `fork.env()` carries `TRAZA_FORK_ID`,
`TRAZA_FORK_PARENT`, `TRAZA_FORK_STEP` and `TRAZA_FORK_RUN` for a subprocess
agent along with the switch's URLs and key. From a shell:

```sh
traza fork weather-7 --at 1 --tool-result '{"forecast": "sun", "celsius": 24}' -- python agent.py
```

## From TypeScript: `client.fork`

```ts
const fork = await traza.fork("weather-7", { atStep: 1, toolResult: { forecast: "sun", celsius: 24 } });
const openai = new OpenAI(fork.openaiOptions());   // { baseURL, apiKey, fetch }
await runAgent(openai);
fork.close();
```

The switch is the `fetch` the fork hands the SDK — both the OpenAI and the
Anthropic SDKs take one — so no port is opened: it sends each call to the
replay until it answers `replay_live`, then that call and every later one to
the real provider (`liveOpenAI` / `liveAnthropic` options, else the
environment's base URL and key). `fork.spanProcessor()` returns a
duck-typed OpenTelemetry span processor that stamps the fork's attributes and
session id on every span started until `fork.close()`.

## Tape tests

A recorded run is a regression test waiting to happen. Replay it strictly and
the agent either walks the same path or says where it left it — with zero
model calls. When a strict replay refuses a request, the refusal now carries
**where the request first differs** from the recording:

```json
{"error":{"type":"replay_divergence","step":0,
  "difference":{"message":1,"role":"user","field":"content","at_char":22,
                "recorded":"What's the weather in Paris?","got":"What's the weather in Lyon?",
                "recorded_messages":2,"got_messages":2},
  "expected_digest":"8b9a07d4aac0a133c789406dfccd9477618c1049ca0255f58b262fc72fca3d8b",
  "got_digest":"1ae84ec741143bc4906084e3b81b9edcea06fb65c14ad097fde00c8afe9bb01f",
  "message":"this request's messages do not match recorded step 0: …"}}
```

A replay holds only each recorded request's digest, so the recorded request
is read again from the step's own span, under the span's tenant and the query
budget, and compared message by message under the same normalisation strict
matching uses. `field` is the first of `role`, `content`, `tool_call_id`,
`tool_calls` that differs — or `missing` / `extra` when one side has fewer
messages — and each side is quoted (120 characters) from just before
`at_char`. The first divergence keeps its difference in the replay's status.
When the recorded request cannot be re-read (erased, offloaded and gone, not
a message list, or past the budget) there is no `difference`, and the digests
are all there is.

The status also counts `exhausted_calls`: model calls refused because every
recorded step had been served — an agent that asked for more than the
recording holds.

### Python and pytest

```python
from traza import Client
from traza.testing import replay_test

def test_weather_agent_follows_its_recording():
    replay_test(Client.from_env(), "weather-7", lambda tape: run_agent("What's the weather in Paris?"))
```

`replay_test` points `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` at a strict
replay while the agent runs, then raises `TapeMismatch` — an
`AssertionError` — when the agent diverged, stopped early, or overran:

```text
the agent diverged from recorded run weather-7 at step 0:
  message 1 (user): content differs at character 22
    recorded: "What's the weather in Paris?"
    got:      "What's the weather in Lyon?"
  (the recording's request has 2 messages, the agent's 2)
```

The package also registers **pytest-traza** (a `pytest11` entry point; with
the source tree alone, `-p traza.pytest_plugin`):

```python
import pytest

@pytest.mark.traza_tape("weather-7")
def test_follows_the_recording(traza_tape):
    run_agent("What's the weather in Paris?")   # every model call answered from the run
```

A marked test runs against a strict replay of the run and fails with the
message above when the agent leaves the path; `mode="loose"` serves in order
regardless. The `traza_tape` fixture is the `TapeTest` (recorded tool results
via `tool_result()`, the environment for a subprocess agent). The server is
`--traza-url` or `TRAZA_URL`, the tenant `--traza-tenant` or
`TRAZA_TENANT`, the credential `TRAZA_TOKEN` — never a command-line option,
where it would land in process listings and CI logs. The plugin needs pytest;
the package itself still depends on nothing.

### TypeScript: vitest, jest, node:test

```ts
import { Client, replayTest } from "@trazadb/sdk";

test("the weather agent follows its recording", async () => {
  await replayTest(new Client(url), "weather-7", async (replay) => {
    await runAgent(new OpenAI({ baseURL: replay.openaiBaseUrl, apiKey: "traza" }));
  });
});
```

`replayTest` throws a `TapeMismatch` with the same message, which every test
runner reports as a failure; an error the agent threw because it was refused
is its `cause`.

## Golden runs and `traza test`

A **golden run** is a run marked as a regression baseline, one of two ways:

- **recorded so** — any of its spans carries `traza.golden = true` (the
  boolean or the string);
- **marked after the fact** — an annotation named `traza.golden` on its
  session (`session_id`), or on its trace with no span (which names the run
  `trace:<trace_id>`). The newest such annotation decides, so a later
  `false` un-marks a run whatever its spans say. `traza golden add RUN` /
  `remove RUN` and `client.mark_golden(run)` / `markGolden(run)` write it
  (an `rw` credential).

`GET /v1/goldens` lists them, ordered by tenant then run so a report reads
the same every time:

```json
{"runs":[{"run":"weather-7","tenant":"","source":"annotation","tagged_at_ns":1792298442062521000}],
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

The exit status is 0 when every run passed and 1 otherwise. **An empty suite
is a failure** unless `--allow-empty` is given: a CI step that tests nothing
should not pass. `--run RUN` (repeatable) tests named runs instead of the
golden ones.

## From an agent: `list_forks`

With `--mcp`, the read tool `list_forks` puts the fork listing in front of a
model — each fork's cut, outcome, cost and where it parted — inside the
untrusted-telemetry block like every stored string on that surface. There is
deliberately no tool that creates a fork, for the same reason there is none
that creates a replay ([MCP guide](mcp.md#the-tools)).

## Limits, stated plainly

- **Traza does not go live; your harness does.** The server stops serving at
  the live point and answers `409 replay_live`. The clients switch to the
  real provider; a harness of your own must do the same on that answer.
- **After an edit without a live point, the recording plays on loosely**,
  answering a conversation it was not recorded in. That exercises the agent's
  own code down the recorded path; it is not what a model would have said.
- **An edited answer has an invented response id and zero usage**, and is
  served under the recording's model name.
- **A fork is a replay**: it lives an hour, in memory, counts against the
  same [registry bounds](replay.md#limits-stated-plainly), and an erasure of
  what it holds withdraws it. Its edits are your data, held as long as it is.
- **The divergence explanation is best effort**: it re-reads the recorded
  span, and says nothing when it cannot.
- **Golden markers are session or trace annotations, or span attributes.** An
  annotation on a single span marks nothing.
- **Bounds:** 64 edits per fork; a fork listing examines 10,000 tagged spans
  and lists at most 50 forks, each diffed within the differ's own caps (5,000
  steps, 50,000 spans a side); a golden listing examines 20,000 tagged spans
  and 10,000 annotations and lists at most 1,000 runs (200 by default).
- **The Python switch and span tagging act on the process.** The environment
  variables are restored when the block ends; a provider client built before
  it, or an instrumentation that sets its own `session.id` after a span
  starts, is not redirected or re-tagged.
- **pytest-traza loads wherever the package is installed.** For an unmarked
  test it does nothing.
