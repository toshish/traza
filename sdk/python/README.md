traza — a Python client for TrazaDB's agent-native APIs
========================================================

A typed client for the parts of [Traza](../../README.md) an agent harness
talks to: the [run fuses](../../docs/guide/guard.md) and the pre-call guard,
[tapes and replays](../../docs/guide/replay.md), diffs, recall, and the
[ledger and evidence packs](../../docs/guide/ledger.md).

It is **not instrumentation**. OpenTelemetry — or OpenLLMetry, or
OpenInference on top of it — records the spans; [`otel_env`](#sending-traces)
writes its exporter settings for you. This package asks the server about what
was recorded, and acts on the answer.

- Python 3.9 or newer.
- No dependencies: `urllib` and `json` from the standard library.
- Typed: dataclasses for every response, and a `py.typed` marker.

Install
-------

From this repository:

```sh
pip install ./sdk/python
```

The tests drive a real `traza-server`; see [Testing](#testing).

Connecting
----------

```python
from traza import Client

traza = Client("http://127.0.0.1:8080", token="trz_…", tenant=None, timeout=10)
traza = Client.from_env()   # TRAZA_URL, TRAZA_TOKEN, TRAZA_TENANT
```

`token` is sent as `Authorization: Bearer`; leave it out for an open server.
The client follows no redirect, so the token goes only to the URL you give
it: a `3xx` (from a proxy upgrading the scheme, say) raises `TrazaError` with
the `Location` in its message. `tenant` scopes every call that takes one, for
an unbound credential. A bound credential can leave it out (the server scopes
it to its own tenant) or name its own tenant; naming any other is refused
with `AuthError` (`403`), and that includes `""`, the default tenant.
`from_env()` treats a variable set to the empty string as unset, as the shell
hook does, so a stray `TRAZA_TENANT=` names no tenant. The `repr` of a client
never shows the token.

```python
>>> traza.whoami()
Identity(auth='open', scope='admin', tenant=None, operator=True, credential=None)
```

Guarding an agent loop
----------------------

A fuse's verdict on an ingest acknowledgement always arrives at least one
step late — the span of a step reaches Traza after the step ran. The pre-call
check is the only surface that can refuse a step **before** it happens, so
that is what the guard asks. (Why, in full:
[the ack is the brake](../../docs/guide/guard.md#the-ack-is-the-brake--and-it-brakes-the-next-step).)

`guard(run, ...)` is a context manager and a decorator. It calls
`check(run, ...)` before the body and raises `FuseTripped` on `stop`, so the
body never starts:

```python
from traza import Client, FuseTripped

traza = Client.from_env()

def run_agent(run_id: str, task: str) -> str:
    state = {"run_id": run_id, "messages": [task], "done": False}
    try:
        while not state["done"]:
            with traza.guard(run_id, est_cost_usd=0.02):
                state = call_model(state)
            for call in state.get("tool_calls", []):
                with traza.guard(run_id, tool=call["name"]):
                    state = run_tool(state, call)
    except FuseTripped as stop:
        # stop.verdict is the whole verdict document: reasons, usage, limits, policy.
        return f"stopped by Traza: {'; '.join(stop.reasons)}"
    return state["answer"]
```

The run id must be the id your instrumentation records the run under — the
span's `session.id` (or `gen_ai.conversation.id`, or OpenLLMetry's
`traceloop.association.properties.session_id`, set with
`Traceloop.set_association_properties({"session_id": run_id})`) — so that the
guard and the recorded spans talk about the same run.

For a graph whose nodes take the state (LangGraph-style), the decorator form
names the run from the node's arguments:

```python
@traza.guard(lambda state, config: config["configurable"]["thread_id"])
def agent_node(state, config):
    ...

@traza.guard(lambda state, config: config["configurable"]["thread_id"], tool="search")
async def search_node(state, config):   # coroutines are guarded too
    ...
```

What a refusal looks like, from a real run where a support agent called
`search_orders` with the same arguments four times under a `max_repeats: 4`
stop policy:

```text
FuseTripped: run 'sess-7f3a' is stopped: max_repeats 4 reached: tool:search_orders#54985dc3c12fada7 repeated 4 times
```

`check()` is the same question without the exception — a `stop` there is an
ordinary answer:

```python
verdict = traza.check("sess-9c21", est_cost_usd=0.40)
verdict.verdict      # 'continue' | 'warn' | 'stop'
verdict.reasons      # ['max_cost_usd 2 would be exceeded by the next step: $1.7 spent + $0.4 estimated']
verdict.usage.cost_usd, verdict.limits.max_cost_usd
```

**If Traza cannot be reached**, the guard raises `TrazaError` (with
`status=None`): running unguarded is a decision, so it is not made for you.
The same goes for an answer the guard cannot act on — a proxy's HTML page, a
body cut short, JSON without a `continue`, `warn` or `stop` verdict — which
is a `TrazaError` too. `guard(run, fail_open=True)` makes the decision for
all of them: the body runs and the error is left on the guard's `.error`.

### Operating fuses

```python
traza.put_policy("support", {"max_cost_usd": 2.0, "max_steps": 40, "max_repeats": 4},
                 service="support-agent", action="stop")
traza.policies()                      # Policies(policies=[…], global_policy=GlobalPolicy(…))
traza.run_verdict("sess-7f3a")        # the run's verdict, usage and limits
traza.runs("stop")                    # stopped runs, most recent first
traza.trip("sess-9c21", reason="customer escalated; take over by hand")   # the kill switch
traza.reset("sess-7f3a", grant={"max_repeats": 8})                       # clear a stop, raise a limit
traza.delete_policy("fp_58ed88d43fc762ba")
```

Limits can be a dict or a `traza.Limits`. Trip, reset and policy writes need an
`rw` credential; `check` needs only `ro`.

### Watching the event feed

```python
stream = traza.events(follow=True)    # every read long-polls (25 s); never ends
for event in stream:
    if event.kind in ("stop", "manual_stop"):
        page_oncall(event.run, event.reasons)
    save_cursor(stream.cursor)        # resume later with traza.events(after=cursor)
```

A following stream's `wait_ms` defaults to 25,000 and may be anything from 1
to 30,000; `follow=True, wait_ms=0` raises `ValueError`, because it would ask
again the instant each empty read came back. Without `follow`, `wait_ms`
defaults to 0 and iteration ends at the first read that returns nothing.
`stream.gap` turns true if the server could no longer honour your cursor —
the feed is a ring of 4,096 events, and a restart empties it.

Claude Code: a hook in Python
-----------------------------

[`examples/claude-code`](../../examples/claude-code/README.md) is the tested,
dependency-free hook pack — the gate, a person's nudges, a summary of earlier
runs at session start, and an installer. The same pre-call check in Python, as
a `PreToolUse` hook command:

```python
#!/usr/bin/env python3
import json, sys
from traza import Client, TrazaError

hook = json.load(sys.stdin)              # Claude Code's hook input
try:
    verdict = Client.from_env().check(hook["session_id"], tool=hook.get("tool_name"))
except TrazaError as error:
    print(f"traza: guard unavailable, allowing the call: {error}", file=sys.stderr)
    sys.exit(0)                          # fail open, as the pack does by default
if verdict.stopped:
    print("Traza run fuse: " + "; ".join(verdict.reasons), file=sys.stderr)
    sys.exit(2)                          # Claude Code blocks the call and shows stderr to the model
```

It checks only. The pack's optional `PostToolUse` recorder also records each
call as a span, which is what gives the session's step and repeat limits
something to count.

Replaying a run with zero model calls
-------------------------------------

`replay(run)` snapshots a recorded run. Point the agent's OpenAI or Anthropic
SDK at it and every model call is answered from the recording — your code,
prompts and control flow run for real; no request goes to a provider.

```python
import json
from openai import OpenAI
from traza import Client

TOOLS = [{"type": "function", "function": {"name": "get_weather", "parameters": {
    "type": "object", "properties": {"city": {"type": "string"}}}}}]

traza = Client.from_env()
replay = traza.replay("weather-7")       # strict by default
llm = OpenAI(base_url=replay.openai_base_url, api_key=traza.token or "traza-open")

messages = [{"role": "system", "content": "You are a weather assistant."},
            {"role": "user", "content": "What's the weather in Paris?"}]
while True:
    answer = llm.chat.completions.create(model="gpt-4o", messages=messages, tools=TOOLS)
    message = answer.choices[0].message
    messages.append(message.model_dump(exclude_none=True))
    if not message.tool_calls:
        break
    for call in message.tool_calls:
        recorded = replay.next_tool(call.function.name)   # the recorded result, not a live call
        messages.append({"role": "tool", "tool_call_id": call.id, "content": json.dumps(recorded.result)})

print(message.content)        # It's raining in Paris, 12°C. Take an umbrella.
print(replay.status())        # served=2, remaining=0, diverged=None, …
```

That loop was run as written against a recorded OpenLLMetry run, with
`openai` 2.33 and, through `replay.anthropic_base_url`, `anthropic` 0.97.

- `replay.env(api_key=None)` gives `OPENAI_BASE_URL`, `OPENAI_API_KEY`,
  `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` as absolute URLs, for an agent
  that builds its own client from the environment:
  `subprocess.run(["python", "agent.py"], env={**os.environ, **replay.env(api_key=agent_key)})`.
  The keys are `api_key`, else your client's Traza token (the Anthropic SDK's
  `x-api-key` is accepted on the replay routes); on an open server a
  placeholder stands in, because the SDKs refuse to start without a key.
- **Hand the agent its own key.** Every library in the agent process can read
  what `env()` exports, and one that reads `OPENAI_API_KEY` but ignores
  `OPENAI_BASE_URL` sends it to the real provider. If your client holds an
  operator token, mint a dedicated `ro` credential bound to the run's tenant,
  create the replay under that tenant (`Client(..., tenant="acme")`), and
  pass the `ro` token as `api_key` (in the loop above, as the OpenAI
  client's `api_key`). Any credential of the replay's tenant may consume it.
- **Tools still run live** unless your harness takes their results from
  `next_tool()`, which returns `None` once no recorded tool step is left.
- In strict mode a request whose messages differ from the recording is refused
  with `409` and `x-should-retry: false`, so the OpenAI SDK raises its own
  `ConflictError` at once instead of retrying; `replay.status().diverged`
  names the step. `mode="loose"` serves the recorded steps in order
  regardless.

Comparing runs, and recalling earlier ones
-----------------------------------------

```python
diff = traza.diff("weather-7", "weather-8")
diff.first_divergence.explanation
# 'step 0 of a and step 0 of b (llm:gpt-4o-2024-08-06) received different input: request 8b9a07d4aac0 in a, 1ae84ec74114 in b'
for step in diff.aligned:
    print(step.op, step.why, step.fingerprint_a, step.fingerprint_b)
diff.delta.cost_usd, diff.delta.steps        # b minus a

for run in traza.recall("weather Paris", outcome="any", limit=5):
    print(run.run, run.outcome, run.score, " -> ".join(run.skeleton))
# weather-7 success 3 llm:gpt-4o-2024-08-06 -> tool:get_weather -> llm:gpt-4o-2024-08-06
```

`tape(run, resolve=False)` returns the run's steps with their inputs, outputs,
digests and token counts.

The ledger and evidence packs
-----------------------------

```python
traza.ledger_head()     # LedgerHead(seq=5, head='3488…', store_id='147c…', at_ns=1792222899559863000)
pack = traza.evidence(since=start_ns, until=end_ns)
pack.verify_digest()    # True: recomputes the digest from the bytes the server sent
```

`verify_digest()` is the check in the
[ledger guide](../../docs/guide/ledger.md#evidence-packs): the SHA-256 of the
canonical JSON of every field but `digest`, with each non-integer number kept
exactly as the server printed it.

Sending traces
--------------

```python
import os
from traza import otel_env

os.environ.update(otel_env("http://127.0.0.1:8080", token="trz_…", tenant="acme"))
# then initialise OpenTelemetry / OpenLLMetry as usual
```

```python
{'OTEL_EXPORTER_OTLP_TRACES_ENDPOINT': 'http://127.0.0.1:8080/v1/traces',
 'OTEL_EXPORTER_OTLP_TRACES_PROTOCOL': 'http/protobuf',
 'OTEL_EXPORTER_OTLP_TRACES_HEADERS': 'Authorization=Bearer%20trz_…',
 'OTEL_RESOURCE_ATTRIBUTES': 'traza.tenant=acme'}
```

These are the trace-signal variables, so an SDK with metrics or logs
exporters on does not send those to Traza, which ingests traces only.
`OTEL_RESOURCE_ATTRIBUTES` replaces any value you already export; merge it by
hand if you set others.

Errors
------

| Exception | When |
|---|---|
| `TrazaError` | Any refusal, and `status=None` when no response arrived. Also a redirect (its `3xx` status; none is followed) and an answer that cannot be read — not JSON, not the object the route answers, cut short — with the text that arrived on `.body`. `.status`, `.message`, `.type` (e.g. `replay_divergence`), `.body`, `.retryable` |
| `AuthError` | `401`, or `403` with `.required` naming the missing grant (`rw`, `admin`, `unbound`) |
| `NotFound` | `404` — another tenant's object included |
| `Conflict` | `409` — a run that cannot be replayed, a strict divergence, `--no-fuses` |
| `Gone` | `410` — a replay withdrawn by an erasure, or exhausted |
| `QuotaExceeded` | `429`, with `.retry_after` in seconds and `.quota` for a quota or pause (`None` when a replay was refused for want of room) |
| `FuseTripped` | Raised by `guard` on `stop`. **Not** a `TrazaError`: a stop is an answer, not a failed request |

Routes this client does not wrap are reachable through
`traza.request(method, path, query=None, body=None)`, with the same
credential and error mapping.

Testing
-------

```sh
cargo build --release      # or point TRAZA_SERVER_BIN at a built traza-server
python3 -m unittest discover -s sdk/python/tests
```

The suite starts two real servers on ephemeral ports — one open, one with
`TRAZA_TOKENS` — and runs every method against their responses, including a
fuse stop raising `FuseTripped`, a replay consumed over the OpenAI and
Anthropic routes with plain `urllib`, a quota refusal, and an evidence pack's
digest. Redirects and unreadable answers, which the real server never sends,
come from a canned-answer stub (`tests/_stub.py`). It takes a few seconds.
